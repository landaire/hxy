//! In-memory record of which permissions the user has granted to
//! each plugin. The host crate owns the data structure; the app
//! crate owns persistence (typically the same SQLite database
//! that backs window settings, app settings, etc.).
//!
//! Plugins are keyed by a [`PluginKey`] = `(name, version, sha256)`
//! so a plugin with the same name but a swapped binary re-prompts.
//! Granted permissions live in [`PermissionGrants`], whose shape
//! mirrors [`crate::manifest::Permissions`]; the host treats a
//! requested-but-not-granted permission identically to "not
//! requested" -- i.e. the linker simply omits the corresponding
//! interface.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use crate::manifest::Permissions;

/// Stable identity of a plugin binary. Two plugins compare equal
/// only when name *and* version *and* content hash match -- a
/// recompile that doesn't change the manifest version still
/// re-triggers consent because `sha256` differs.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PluginKey {
    pub name: String,
    pub version: String,
    /// Lowercase-hex SHA-256 of the `.wasm` bytes. Built via
    /// [`PluginKey::sha256_hex`] for consistency.
    pub sha256: String,
}

impl PluginKey {
    /// Derive a key from manifest identity + the plugin's bytes.
    pub fn from_bytes(name: impl Into<String>, version: impl Into<String>, wasm: &[u8]) -> Self {
        Self { name: name.into(), version: version.into(), sha256: Self::sha256_hex(wasm) }
    }

    /// Hex-encoded SHA-256 of `bytes`. Lowercase, no separator;
    /// matches `sha256sum` output ahead of the filename.
    pub fn sha256_hex(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        let digest = h.finalize();
        let mut out = String::with_capacity(digest.len() * 2);
        for b in digest {
            use std::fmt::Write;
            let _ = write!(&mut out, "{b:02x}");
        }
        out
    }

    /// Compact serialization key. Format: `name@version`. Used as
    /// the map key inside [`PluginGrants`].
    ///
    /// Note that the sha256 is intentionally NOT part of the key:
    /// rebuilding the same plugin (same name, same version) at
    /// different bytes would otherwise wipe the user's grant on
    /// every dev iteration. The sha256 still lives on the embedded
    /// [`PluginKey`] so the consent UI can surface a fingerprint
    /// mismatch if we want to warn on substituted bytes; bumping
    /// the version is the explicit "this is a different plugin
    /// now, please re-consent" signal.
    fn map_key(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }
}

/// Per-permission user decisions. Mirror of [`Permissions`]; absent
/// entries default to denied.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionGrants {
    pub persist: bool,
    pub commands: bool,
    /// Subset of [`Permissions::network`] the user has approved.
    /// Each entry must appear verbatim in the manifest's
    /// requested list -- the consent UI surfaces the requested
    /// patterns as checkboxes; the user can toggle each but
    /// cannot type free-form patterns the plugin author didn't
    /// declare. Patterns not present here are denied at
    /// `tcp.connect` time.
    pub network: Vec<String>,
}

impl PermissionGrants {
    /// Restrict `requested` to the permissions actually granted.
    /// Used at linker-wiring time: the host walks the manifest's
    /// declared permissions through this filter so a plugin that
    /// asked for `persist` but was denied gets the same treatment
    /// as one that never asked. For `network`, the result is the
    /// patterns that appear in *both* the requested list and the
    /// granted list -- a granted pattern that is no longer in the
    /// manifest (plugin upgrade dropped it) silently disappears.
    pub fn intersect(&self, requested: &Permissions) -> Permissions {
        Permissions {
            persist: self.persist && requested.persist,
            commands: self.commands && requested.commands,
            network: requested.network.iter().filter(|p| self.network.iter().any(|g| g == *p)).cloned().collect(),
        }
    }
}

/// In-memory grants store. Implements `Serialize` / `Deserialize`
/// so the app crate can blob the whole thing into its existing
/// kv layer (SQLite); the host crate doesn't pick the storage.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginGrants {
    /// Flat map keyed by [`PluginKey::map_key`]; the embedded
    /// `PluginKey` carries the canonical identity so a corrupted
    /// or hand-edited heading doesn't desynchronize from the body.
    #[serde(default)]
    plugins: BTreeMap<String, PluginGrantEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PluginGrantEntry {
    key: PluginKey,
    grants: PermissionGrants,
}

impl PluginGrants {
    /// Look up the recorded decisions for `key`. Returns
    /// [`PermissionGrants::default`] (all false) when no record
    /// exists yet -- the caller should treat that as "needs consent."
    /// Cloned to keep the borrow chain short for the typical
    /// startup loop, which iterates plugins and intersects each
    /// against its manifest; the per-plugin allowlist Vec is
    /// short enough that cloning is cheaper than rearranging
    /// lifetimes.
    pub fn get(&self, key: &PluginKey) -> PermissionGrants {
        self.plugins.get(&key.map_key()).map(|e| e.grants.clone()).unwrap_or_default()
    }

    /// Whether we have *any* recorded decision for `key`. Distinct
    /// from "all permissions denied" because the latter is a real
    /// user choice we should honor without re-prompting.
    pub fn has_record(&self, key: &PluginKey) -> bool {
        self.plugins.contains_key(&key.map_key())
    }

    /// Record (or overwrite) the user's decisions for `key`.
    pub fn set(&mut self, key: PluginKey, grants: PermissionGrants) {
        let map_key = key.map_key();
        self.plugins.insert(map_key, PluginGrantEntry { key, grants });
    }

    /// Forget any record for `key`. The next load that observes
    /// this plugin will treat it as needing fresh consent. Returns
    /// `true` when an entry was actually removed -- callers (e.g.
    /// the uninstall flow) use this to skip a needless re-persist
    /// when the grant table is unchanged.
    pub fn forget(&mut self, key: &PluginKey) -> bool {
        self.plugins.remove(&key.map_key()).is_some()
    }

    /// Iterate every (key, grants) pair currently recorded. Used
    /// by the consent UI to render existing decisions.
    pub fn iter(&self) -> impl Iterator<Item = (&PluginKey, &PermissionGrants)> {
        self.plugins.values().map(|e| (&e.key, &e.grants))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vector() {
        // Empty input has a well-known SHA-256 digest -- if this
        // changes we've picked up a mis-wired hasher.
        let h = PluginKey::sha256_hex(b"");
        assert_eq!(h, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }

    #[test]
    fn intersect_drops_denied_permissions() {
        let granted = PermissionGrants { persist: true, commands: false, network: vec![] };
        let requested = Permissions { persist: true, commands: true, network: vec![] };
        let actual = granted.intersect(&requested);
        assert!(actual.persist);
        assert!(!actual.commands);
    }

    #[test]
    fn intersect_drops_unrequested_permissions() {
        let granted = PermissionGrants { persist: true, commands: true, network: vec![] };
        let requested = Permissions { persist: false, commands: true, network: vec![] };
        let actual = granted.intersect(&requested);
        assert!(!actual.persist);
        assert!(actual.commands);
    }

    #[test]
    fn intersect_keeps_only_overlapping_network_patterns() {
        let granted = PermissionGrants {
            persist: false,
            commands: false,
            // User left two of the three originally-requested patterns
            // checked. The third (`evil.example.com:*`) shouldn't
            // leak through, and a granted pattern that was dropped
            // from the manifest in a plugin upgrade
            // (`old-removed:80`) shouldn't either.
            network: vec!["192.168.1.50:730".into(), "*:443".into(), "old-removed:80".into()],
        };
        let requested = Permissions {
            persist: false,
            commands: false,
            network: vec!["192.168.1.50:730".into(), "*:443".into(), "evil.example.com:*".into()],
        };
        let actual = granted.intersect(&requested);
        assert_eq!(actual.network, vec!["192.168.1.50:730".to_string(), "*:443".to_string()]);
    }

    #[test]
    fn json_roundtrip_preserves_records() {
        let mut g = PluginGrants::default();
        let key = PluginKey::from_bytes("xeedee", "0.1.0", b"\0asm\x01\x00\x00\x00");
        g.set(key.clone(), PermissionGrants { persist: true, commands: false, network: vec![] });

        let json = serde_json::to_string(&g).expect("serialize");
        let loaded: PluginGrants = serde_json::from_str(&json).expect("deserialize");
        assert!(loaded.has_record(&key));
        assert_eq!(loaded.get(&key), PermissionGrants { persist: true, commands: false, network: vec![] });
    }

    #[test]
    fn forget_removes_record() {
        let mut g = PluginGrants::default();
        let key = PluginKey::from_bytes("xeedee", "0.1.0", b"hello");
        g.set(key.clone(), PermissionGrants { persist: true, commands: true, network: vec![] });
        assert!(g.has_record(&key));
        g.forget(&key);
        assert!(!g.has_record(&key));
    }

    #[test]
    fn rebuilt_plugin_at_same_version_carries_grants_over() {
        // Map keys are name@version (sha256 intentionally NOT in
        // the lookup): rebuilding the same plugin shouldn't wipe
        // the user's consent on every dev iteration. Bumping the
        // version (or renaming) is the explicit re-consent signal.
        let k1 = PluginKey::from_bytes("p", "1.0.0", b"first");
        let k2 = PluginKey::from_bytes("p", "1.0.0", b"second");
        assert_ne!(k1, k2, "PluginKey identity still differs by sha256");
        let mut g = PluginGrants::default();
        g.set(k1.clone(), PermissionGrants { persist: true, commands: false, network: vec![] });
        assert!(g.has_record(&k1));
        assert!(g.has_record(&k2), "grant should carry across rebuild");
    }

    #[test]
    fn version_bump_drops_grants() {
        let k1 = PluginKey::from_bytes("p", "1.0.0", b"first");
        let k2 = PluginKey::from_bytes("p", "1.1.0", b"first");
        let mut g = PluginGrants::default();
        g.set(k1.clone(), PermissionGrants { persist: true, commands: false, network: vec![] });
        assert!(g.has_record(&k1));
        assert!(!g.has_record(&k2), "version bump should require fresh consent");
    }

    #[test]
    fn distinct_names_distinct_keys() {
        let k1 = PluginKey::from_bytes("alpha", "1.0.0", b"same");
        let k2 = PluginKey::from_bytes("beta", "1.0.0", b"same");
        let mut g = PluginGrants::default();
        g.set(k1.clone(), PermissionGrants { persist: true, commands: false, network: vec![] });
        assert!(g.has_record(&k1));
        assert!(!g.has_record(&k2));
    }
}
