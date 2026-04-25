//! Per-plugin opaque blob storage. The host crate provides the
//! [`StateStore`] *trait*; concrete backends (SQLite in the app
//! crate, in-memory for tests) live wherever the storage tech is
//! already established. Keeping the trait here keeps the host
//! crate independent of any one persistence stack.
//!
//! The contract is intentionally minimal: load / save / clear
//! against an opaque per-plugin name, with a host-imposed quota
//! the plugin cannot influence. Sized for "remember IP + token +
//! a small list of recents"; if a real plugin needs more we
//! reopen this conversation rather than letting plugins ask for
//! arbitrary disk.

use std::collections::HashMap;
use std::sync::Mutex;

use thiserror::Error;

/// Per-plugin maximum blob size in bytes. Enforced by every
/// [`StateStore`] implementation; plugins cannot raise the cap
/// from the manifest.
pub const MAX_STATE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("plugin name {name:?} contains characters disallowed in a storage key")]
    InvalidName { name: String },
    #[error("blob is {actual} bytes but the per-plugin quota is {limit}")]
    QuotaExceeded { actual: u64, limit: u64 },
    #[error("backing store I/O")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Opaque per-plugin storage backend. Implementations must be
/// thread-safe (`Send + Sync`) because the host can drive plugin
/// calls from multiple threads concurrently.
///
/// Names arrive as the plugin's manifest name. Implementations
/// should reject anything that would let a plugin escape its
/// namespace (path separators, NUL bytes, dot-traversal); the
/// helper [`validate_plugin_name`] enforces a uniform policy and
/// is recommended for any backend that uses the name as part of
/// a key.
pub trait StateStore: Send + Sync {
    /// Previously-saved blob, if any. `Ok(None)` when no blob
    /// exists (fresh install or after a `clear`).
    fn load(&self, plugin_name: &str) -> Result<Option<Vec<u8>>, StateError>;

    /// Replace the stored blob. Implementations must enforce the
    /// [`MAX_STATE_BYTES`] quota by returning
    /// [`StateError::QuotaExceeded`] before any I/O.
    fn save(&self, plugin_name: &str, blob: &[u8]) -> Result<(), StateError>;

    /// Drop the stored blob. No-op when nothing is saved.
    fn clear(&self, plugin_name: &str) -> Result<(), StateError>;
}

/// Validate a plugin name against the policy every backend should
/// share. Rejects empty strings, dot-traversal, and anything that
/// could escape a name-keyed storage layout.
pub fn validate_plugin_name(name: &str) -> Result<(), StateError> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name == "."
        || name == ".."
    {
        return Err(StateError::InvalidName { name: name.to_owned() });
    }
    Ok(())
}

/// Process-local in-memory backend. Useful for tests and as the
/// fallback when the host has no real persistence wired up
/// (anything saved to it dies with the process).
#[derive(Debug, Default)]
pub struct InMemoryStateStore {
    inner: Mutex<HashMap<String, Vec<u8>>>,
}

impl InMemoryStateStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl StateStore for InMemoryStateStore {
    fn load(&self, plugin_name: &str) -> Result<Option<Vec<u8>>, StateError> {
        validate_plugin_name(plugin_name)?;
        Ok(self.inner.lock().expect("poisoned").get(plugin_name).cloned())
    }

    fn save(&self, plugin_name: &str, blob: &[u8]) -> Result<(), StateError> {
        validate_plugin_name(plugin_name)?;
        let actual = blob.len() as u64;
        if actual > MAX_STATE_BYTES {
            return Err(StateError::QuotaExceeded { actual, limit: MAX_STATE_BYTES });
        }
        self.inner.lock().expect("poisoned").insert(plugin_name.to_owned(), blob.to_vec());
        Ok(())
    }

    fn clear(&self, plugin_name: &str) -> Result<(), StateError> {
        validate_plugin_name(plugin_name)?;
        self.inner.lock().expect("poisoned").remove(plugin_name);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_returns_none() {
        let s = InMemoryStateStore::new();
        assert_eq!(s.load("absent").unwrap(), None);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let s = InMemoryStateStore::new();
        s.save("xeedee", b"hello, plugin").unwrap();
        assert_eq!(s.load("xeedee").unwrap().as_deref(), Some(&b"hello, plugin"[..]));
    }

    #[test]
    fn save_replaces_previous_blob() {
        let s = InMemoryStateStore::new();
        s.save("xeedee", b"first").unwrap();
        s.save("xeedee", b"second").unwrap();
        assert_eq!(s.load("xeedee").unwrap().as_deref(), Some(&b"second"[..]));
    }

    #[test]
    fn clear_removes_blob() {
        let s = InMemoryStateStore::new();
        s.save("xeedee", b"x").unwrap();
        s.clear("xeedee").unwrap();
        assert_eq!(s.load("xeedee").unwrap(), None);
    }

    #[test]
    fn clear_missing_is_noop() {
        let s = InMemoryStateStore::new();
        s.clear("absent").unwrap();
    }

    #[test]
    fn quota_is_enforced() {
        let s = InMemoryStateStore::new();
        let blob = vec![0u8; (MAX_STATE_BYTES + 1) as usize];
        let err = s.save("p", &blob).unwrap_err();
        match err {
            StateError::QuotaExceeded { actual, limit } => {
                assert!(actual > limit);
                assert_eq!(limit, MAX_STATE_BYTES);
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn invalid_name_rejected() {
        let s = InMemoryStateStore::new();
        for bad in ["", ".", "..", "a/b", "a\\b", "a\0b"] {
            let err = s.save(bad, b"x").unwrap_err();
            assert!(matches!(err, StateError::InvalidName { .. }), "name {bad:?}: {err:?}");
        }
    }
}
