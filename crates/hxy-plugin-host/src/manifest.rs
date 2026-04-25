//! Plugin manifest -- a `<plugin>.hxy.toml` sidecar describing the
//! plugin's identity and the host-provided capabilities it wants.
//!
//! Manifests live next to the `.wasm` component and are loaded at
//! plugin discovery time. Permissions declared here are *requests*;
//! the host gates them against [`crate::grants::PluginGrants`] before
//! actually exposing the corresponding interfaces to the plugin
//! instance. A plugin without a sidecar is treated as if it requested
//! no permissions (it can still mount sources -- that's the baseline
//! API every plugin has).

use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

/// Suffix appended to a `.wasm` plugin's stem to find its manifest:
/// `xeedee.wasm` -> `xeedee.hxy.toml`.
pub const MANIFEST_EXTENSION: &str = "hxy.toml";

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("read manifest {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse manifest {path}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

/// Parsed sidecar.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub plugin: PluginMeta,
    /// Host capabilities the plugin requests. Absent in the sidecar
    /// is equivalent to a fully-default `Permissions` (i.e. nothing
    /// requested).
    #[serde(default)]
    pub permissions: Permissions,
}

/// Identity block. `name` is the user-visible label; the host also
/// pairs it with the plugin's content hash to form a stable key for
/// grants and persisted state, so renaming a `.wasm` file does not
/// re-trigger consent but swapping its bytes does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginMeta {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub authors: Vec<String>,
    /// One-line description shown verbatim in the consent dialog.
    /// Plugins that need richer docs should link to a URL.
    #[serde(default)]
    pub description: String,
}

/// Capability flags. Each field corresponds to one host-provided WIT
/// interface or behavior the plugin can use. With the current
/// single-world wiring every interface is always linked; gating
/// happens at the host's interface impl, which returns a denial
/// (or empty list) when the corresponding flag is `false`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Permissions {
    /// Allow the plugin to load and save an opaque per-plugin blob
    /// via the `state` interface. Host enforces a fixed quota;
    /// plugins cannot ask for more.
    pub persist: bool,
    /// Allow the plugin to contribute entries to the command
    /// palette via its exported `commands` interface. Off means
    /// the host never calls `list-commands`, so the plugin is
    /// effectively passive (only acts when the user navigates to
    /// a path it claims via the handler interface).
    pub commands: bool,
    /// Outbound-TCP allowlist: each entry is a `host:port` pattern
    /// the plugin wants to connect to. `*` matches anything in
    /// either field, e.g. `"*:443"` (any host, port 443),
    /// `"github.com:*"` (any port on github.com),
    /// `"192.168.1.50:730"` (one specific endpoint).
    ///
    /// Empty list = no network access requested. Matching is on the
    /// *literal host string* the plugin passes to `tcp.connect` --
    /// the host does not resolve and re-check, so a plugin that
    /// passes `"localhost"` will not match a `"127.0.0.1:*"`
    /// pattern. Plugin authors should declare the names they will
    /// actually use.
    pub network: Vec<String>,
}

impl PluginManifest {
    /// Path the host will read this plugin's manifest from given
    /// the `.wasm` file path. Returns `wasm.with_extension(...)`
    /// where the extension is [`MANIFEST_EXTENSION`].
    pub fn sidecar_path(wasm: &Path) -> PathBuf {
        wasm.with_extension(MANIFEST_EXTENSION)
    }

    /// Read the sidecar next to `wasm` and parse it. Returns `Ok(None)`
    /// when the sidecar simply does not exist -- a missing manifest is
    /// not an error, it just means the plugin requested zero host
    /// capabilities. I/O and parse failures are surfaced.
    pub fn load_for(wasm: &Path) -> Result<Option<Self>, ManifestError> {
        let path = Self::sidecar_path(wasm);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(ManifestError::Read { path, source }),
        };
        let text = String::from_utf8(bytes)
            .map_err(|e| ManifestError::Read { path: path.clone(), source: std::io::Error::other(e) })?;
        let parsed: Self = toml::from_str(&text).map_err(|source| ManifestError::Parse { path, source })?;
        Ok(Some(parsed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_manifest() {
        let toml_src = r#"
[plugin]
name = "xeedee"
version = "0.1.0"
authors = ["lander"]
description = "Browse modules on a remote Xbox 360 over xbdm."

[permissions]
persist = true
commands = true
network = ["xbox.local:730", "*:443"]
"#;
        let m: PluginManifest = toml::from_str(toml_src).expect("parse");
        assert_eq!(m.plugin.name, "xeedee");
        assert_eq!(m.plugin.version, "0.1.0");
        assert_eq!(m.plugin.authors, vec!["lander".to_string()]);
        assert!(m.permissions.persist);
        assert!(m.permissions.commands);
        assert_eq!(
            m.permissions.network,
            vec!["xbox.local:730".to_string(), "*:443".to_string()]
        );
    }

    #[test]
    fn permissions_default_to_none_when_omitted() {
        let toml_src = r#"
[plugin]
name = "passive"
version = "0.0.1"
"#;
        let m: PluginManifest = toml::from_str(toml_src).expect("parse");
        assert_eq!(m.permissions, Permissions::default());
        assert!(!m.permissions.persist);
        assert!(!m.permissions.commands);
    }

    #[test]
    fn sidecar_path_swaps_extension() {
        let p = PluginManifest::sidecar_path(Path::new("/plugins/xeedee.wasm"));
        assert_eq!(p, PathBuf::from("/plugins/xeedee.hxy.toml"));
    }

    #[test]
    fn missing_sidecar_returns_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wasm = dir.path().join("nope.wasm");
        std::fs::write(&wasm, b"\0asm").expect("write wasm");
        let result = PluginManifest::load_for(&wasm).expect("load");
        assert!(result.is_none());
    }

    #[test]
    fn parse_error_surfaces_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wasm = dir.path().join("bad.wasm");
        std::fs::write(&wasm, b"\0asm").expect("write wasm");
        std::fs::write(PluginManifest::sidecar_path(&wasm), "not = valid = toml").expect("write sidecar");
        let err = PluginManifest::load_for(&wasm).expect_err("should fail");
        assert!(matches!(err, ManifestError::Parse { .. }));
    }
}
