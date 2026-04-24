//! On-disk persistence for plugin state blobs. One file per plugin,
//! contents opaque to the host -- the plugin owns the format and is
//! responsible for any versioning. The host's contract is just
//! "atomic save with a quota and namespaced layout."
//!
//! Used by the `hxy:host/state` interface; the linker only wires
//! that interface up when the plugin's manifest declared `persist`
//! and the user granted it.

use std::path::PathBuf;

use thiserror::Error;

/// Per-plugin maximum blob size in bytes. The host imposes this
/// uniformly: plugins cannot self-elevate their quota in the
/// manifest. Sized for "remember IP + token + a small list of
/// recents" -- if a real plugin needs more we reopen this conversation
/// rather than letting plugins ask for arbitrary disk.
pub const MAX_STATE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("plugin name {name:?} contains characters disallowed in a filename")]
    InvalidName { name: String },
    #[error("blob is {actual} bytes but the per-plugin quota is {limit}")]
    QuotaExceeded { actual: u64, limit: u64 },
    #[error("create state directory {path}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("write state file {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("read state file {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Filesystem-backed store rooted at `dir`. Each plugin's blob lives
/// at `<dir>/<plugin_name>.bin`. The directory is created on first
/// `save`; `load` and `clear` tolerate its absence.
#[derive(Clone, Debug)]
pub struct StateStore {
    dir: PathBuf,
    quota: u64,
}

impl StateStore {
    /// Construct a store rooted at `dir` with the default
    /// [`MAX_STATE_BYTES`] quota.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into(), quota: MAX_STATE_BYTES }
    }

    /// Override the quota. Intended for tests; production code
    /// should rely on the default so the limit is one place.
    #[doc(hidden)]
    pub fn with_quota(mut self, quota: u64) -> Self {
        self.quota = quota;
        self
    }

    /// Load the previously-saved blob for `plugin_name`. Returns
    /// `Ok(None)` when no blob exists (fresh install or after a
    /// `clear`).
    pub fn load(&self, plugin_name: &str) -> Result<Option<Vec<u8>>, StateError> {
        let path = self.path_for(plugin_name)?;
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(StateError::Read { path, source }),
        }
    }

    /// Atomically replace the stored blob. Writes to a temp file in
    /// the same directory then renames over the target so a crash
    /// during write leaves either the previous blob or the new one
    /// readable, never a torn write.
    pub fn save(&self, plugin_name: &str, blob: &[u8]) -> Result<(), StateError> {
        let actual = blob.len() as u64;
        if actual > self.quota {
            return Err(StateError::QuotaExceeded { actual, limit: self.quota });
        }
        let path = self.path_for(plugin_name)?;
        std::fs::create_dir_all(&self.dir)
            .map_err(|source| StateError::CreateDir { path: self.dir.clone(), source })?;
        let tmp = path.with_extension("bin.tmp");
        std::fs::write(&tmp, blob).map_err(|source| StateError::Write { path: tmp.clone(), source })?;
        std::fs::rename(&tmp, &path).map_err(|source| StateError::Write { path, source })?;
        Ok(())
    }

    /// Remove the stored blob. No-op when nothing is saved.
    pub fn clear(&self, plugin_name: &str) -> Result<(), StateError> {
        let path = self.path_for(plugin_name)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StateError::Write { path, source }),
        }
    }

    /// Per-plugin quota in bytes. Plugins cannot read this from WIT;
    /// it is exposed for host code that wants to surface the limit
    /// in UI or logs.
    pub fn quota(&self) -> u64 {
        self.quota
    }

    fn path_for(&self, plugin_name: &str) -> Result<PathBuf, StateError> {
        if plugin_name.is_empty()
            || plugin_name.contains('/')
            || plugin_name.contains('\\')
            || plugin_name.contains('\0')
            || plugin_name == "."
            || plugin_name == ".."
        {
            return Err(StateError::InvalidName { name: plugin_name.to_owned() });
        }
        Ok(self.dir.join(format!("{plugin_name}.bin")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, StateStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = StateStore::new(dir.path());
        (dir, s)
    }

    #[test]
    fn load_missing_returns_none() {
        let (_d, s) = store();
        assert_eq!(s.load("absent").unwrap(), None);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let (_d, s) = store();
        s.save("xeedee", b"hello, plugin").unwrap();
        assert_eq!(s.load("xeedee").unwrap().as_deref(), Some(&b"hello, plugin"[..]));
    }

    #[test]
    fn save_replaces_previous_blob() {
        let (_d, s) = store();
        s.save("xeedee", b"first").unwrap();
        s.save("xeedee", b"second").unwrap();
        assert_eq!(s.load("xeedee").unwrap().as_deref(), Some(&b"second"[..]));
    }

    #[test]
    fn clear_removes_blob() {
        let (_d, s) = store();
        s.save("xeedee", b"x").unwrap();
        s.clear("xeedee").unwrap();
        assert_eq!(s.load("xeedee").unwrap(), None);
    }

    #[test]
    fn clear_missing_is_noop() {
        let (_d, s) = store();
        s.clear("absent").unwrap();
    }

    #[test]
    fn quota_is_enforced() {
        let (_d, s) = store();
        let s = s.with_quota(8);
        let err = s.save("p", b"too long for the quota").unwrap_err();
        match err {
            StateError::QuotaExceeded { actual, limit } => {
                assert!(actual > limit);
                assert_eq!(limit, 8);
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn invalid_name_rejected() {
        let (_d, s) = store();
        for bad in ["", ".", "..", "a/b", "a\\b", "a\0b"] {
            let err = s.save(bad, b"x").unwrap_err();
            assert!(matches!(err, StateError::InvalidName { .. }), "name {bad:?}: {err:?}");
        }
    }
}
