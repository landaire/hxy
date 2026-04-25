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

    /// Compact serialization key. Format: `name@version#sha256-prefix`
    /// -- the hash prefix gives a visual distinguisher between
    /// rebuilds at the same version without dumping the full 64-char
    /// digest. Used as the map key inside [`PluginGrants`] so a
    /// JSON dump of the struct stays human-scannable.
    fn map_key(&self) -> String {
        let prefix: String = self.sha256.chars().take(12).collect();
        format!("{}@{}#{}", self.name, self.version, prefix)
    }
}

/// Per-permission user decisions. Mirror of [`Permissions`]; absent
/// entries default to `false` (deny).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionGrants {
    pub persist: bool,
    pub commands: bool,
}

impl PermissionGrants {
    /// Restrict `requested` to the permissions actually granted.
    /// Used at linker-wiring time: the host walks the manifest's
    /// declared permissions through this filter so a plugin that
    /// asked for `persist` but was denied gets the same treatment
    /// as one that never asked.
    pub fn intersect(self, requested: Permissions) -> Permissions {
        Permissions {
            persist: self.persist && requested.persist,
            commands: self.commands && requested.commands,
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
    pub fn get(&self, key: &PluginKey) -> PermissionGrants {
        self.plugins.get(&key.map_key()).map(|e| e.grants).unwrap_or_default()
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
    /// this plugin will treat it as needing fresh consent.
    pub fn forget(&mut self, key: &PluginKey) {
        self.plugins.remove(&key.map_key());
    }

    /// Iterate every (key, grants) pair currently recorded. Used
    /// by the consent UI to render existing decisions.
    pub fn iter(&self) -> impl Iterator<Item = (&PluginKey, PermissionGrants)> {
        self.plugins.values().map(|e| (&e.key, e.grants))
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
        let granted = PermissionGrants { persist: true, commands: false };
        let requested = Permissions { persist: true, commands: true };
        let actual = granted.intersect(requested);
        assert!(actual.persist);
        assert!(!actual.commands);
    }

    #[test]
    fn intersect_drops_unrequested_permissions() {
        let granted = PermissionGrants { persist: true, commands: true };
        let requested = Permissions { persist: false, commands: true };
        let actual = granted.intersect(requested);
        assert!(!actual.persist);
        assert!(actual.commands);
    }

    #[test]
    fn json_roundtrip_preserves_records() {
        let mut g = PluginGrants::default();
        let key = PluginKey::from_bytes("xeedee", "0.1.0", b"\0asm\x01\x00\x00\x00");
        g.set(key.clone(), PermissionGrants { persist: true, commands: false });

        let json = serde_json::to_string(&g).expect("serialize");
        let loaded: PluginGrants = serde_json::from_str(&json).expect("deserialize");
        assert!(loaded.has_record(&key));
        assert_eq!(loaded.get(&key), PermissionGrants { persist: true, commands: false });
    }

    #[test]
    fn forget_removes_record() {
        let mut g = PluginGrants::default();
        let key = PluginKey::from_bytes("xeedee", "0.1.0", b"hello");
        g.set(key.clone(), PermissionGrants { persist: true, commands: true });
        assert!(g.has_record(&key));
        g.forget(&key);
        assert!(!g.has_record(&key));
    }

    #[test]
    fn distinct_hashes_distinct_keys() {
        let k1 = PluginKey::from_bytes("p", "1.0.0", b"first");
        let k2 = PluginKey::from_bytes("p", "1.0.0", b"second");
        assert_ne!(k1, k2);
        let mut g = PluginGrants::default();
        g.set(k1.clone(), PermissionGrants { persist: true, commands: false });
        assert!(g.has_record(&k1));
        assert!(!g.has_record(&k2));
    }
}
