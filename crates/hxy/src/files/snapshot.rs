//! Per-file snapshot capture + comparison support.
//!
//! A snapshot freezes the patched bytes of an open file at a
//! moment in time. The user can capture as many as they want,
//! rename them, delete them, and compare any pair (or one
//! against the live buffer) through the existing CompareSession
//! machinery.
//!
//! Storage strategy: every snapshot is mirrored to a sidecar
//! file on disk so the user can take a snapshot, reload, and
//! still compare back to the pre-reload state across an app
//! restart. Files small enough also keep an in-memory cache so
//! comparisons don't pay a re-read cost on every recompute.

#![cfg(not(target_arch = "wasm32"))]

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use suture::metadata::HashAlgorithm;

use crate::APP_NAME;

/// Largest payload kept in `cached_bytes`. Above this, every
/// read goes back to disk so a long-running session with many
/// snapshots can't inflate the resident set indefinitely.
pub const IN_MEMORY_CACHE_MAX: u64 = 500 * 1024 * 1024;

/// Stable identifier for one snapshot inside a file's snapshot
/// list. Allocated monotonically by [`SnapshotStore`]; survives
/// renames and reorders.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SnapshotId(u64);

impl SnapshotId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }
    pub fn get(self) -> u64 {
        self.0
    }
}

/// A single snapshot of an open file's patched bytes. The
/// `cached_bytes` field is populated when the captured payload
/// fits under [`IN_MEMORY_CACHE_MAX`]; larger snapshots always
/// re-read from `sidecar_path` on demand. Either way the
/// sidecar is the authoritative copy.
#[derive(Clone)]
pub struct Snapshot {
    pub id: SnapshotId,
    pub name: String,
    pub captured_at: jiff::Timestamp,
    pub byte_len: u64,
    pub sidecar_path: PathBuf,
    pub cached_bytes: Option<Arc<Vec<u8>>>,
}

impl Snapshot {
    /// Resolve the snapshot's bytes. Hits the cache when present;
    /// falls back to a fresh disk read otherwise.
    pub fn load_bytes(&self) -> std::io::Result<Arc<Vec<u8>>> {
        if let Some(cached) = &self.cached_bytes {
            return Ok(Arc::clone(cached));
        }
        let bytes = std::fs::read(&self.sidecar_path)?;
        Ok(Arc::new(bytes))
    }

    /// Whether the snapshot was small enough at capture time to
    /// be cached in memory. Surfaced in the snapshot list so the
    /// user can see why a comparison is going to re-read from
    /// disk.
    pub fn is_cached(&self) -> bool {
        self.cached_bytes.is_some()
    }
}

/// Per-file collection of snapshots plus the next-id counter.
/// Lives on [`crate::files::OpenFile`] so each tab tracks its
/// own snapshot history independently.
pub struct SnapshotStore {
    next_id: u64,
    pub snapshots: Vec<Snapshot>,
    /// Directory on disk where sidecars for this file live. All
    /// snapshot bytes are written under here as
    /// `<snapshot-id>.bin`; index metadata is in `index.json`.
    /// `None` when the platform doesn't expose a data dir, in
    /// which case captures only persist for the running session.
    sidecar_dir: Option<PathBuf>,
}

impl SnapshotStore {
    /// Build a fresh in-memory store rooted at the sidecar
    /// directory derived from `key_path`. `key_path` is whatever
    /// uniquely identifies this file (filesystem path for disk-
    /// backed tabs; the source kind's display string for VFS
    /// entries) -- it's hashed to produce a filesystem-safe
    /// directory name so two files with identical names don't
    /// collide.
    pub fn new(key_path: &Path) -> Self {
        Self { next_id: 1, snapshots: Vec::new(), sidecar_dir: snapshot_dir_for(key_path) }
    }

    /// Restore a previously-persisted store. Reads the index
    /// file under `sidecar_dir_for(key_path)`; missing index is
    /// treated as a fresh store.
    pub fn restore(key_path: &Path) -> Self {
        let mut store = Self::new(key_path);
        let Some(dir) = store.sidecar_dir.clone() else { return store };
        let index_path = dir.join("index.json");
        let bytes = match std::fs::read(&index_path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return store,
            Err(e) => {
                tracing::warn!(error = %e, path = %index_path.display(), "read snapshot index");
                return store;
            }
        };
        let parsed: PersistedIndex = match serde_json::from_slice(&bytes) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "parse snapshot index");
                return store;
            }
        };
        store.next_id = parsed.next_id.max(1);
        for entry in parsed.entries {
            let sidecar_path = dir.join(format!("{}.bin", entry.id.get()));
            // Drop entries whose payload file vanished (user
            // deleted manually, partial write last session).
            if !sidecar_path.exists() {
                tracing::debug!(id = entry.id.get(), "snapshot sidecar missing; dropping entry");
                continue;
            }
            store.snapshots.push(Snapshot {
                id: entry.id,
                name: entry.name,
                captured_at: entry.captured_at,
                byte_len: entry.byte_len,
                sidecar_path,
                cached_bytes: None,
            });
        }
        store
    }

    /// Capture `bytes` as a new snapshot with the given
    /// human-readable `name` (defaults to `Snapshot N` when
    /// empty). Returns the new snapshot's id, or an error if
    /// the sidecar write fails.
    pub fn capture(&mut self, name: String, bytes: Vec<u8>) -> std::io::Result<SnapshotId> {
        let id = SnapshotId::new(self.next_id);
        self.next_id += 1;
        let display_name = if name.trim().is_empty() { format!("Snapshot {}", id.get()) } else { name };

        let sidecar_path = match &self.sidecar_dir {
            Some(dir) => {
                std::fs::create_dir_all(dir)?;
                dir.join(format!("{}.bin", id.get()))
            }
            None => {
                // Fall back to a tempfile so the in-memory cache
                // still has a coherent sidecar pointer, even if
                // it won't survive the session.
                let mut path = std::env::temp_dir();
                path.push(format!("hxy-snapshot-{}.bin", id.get()));
                path
            }
        };
        std::fs::write(&sidecar_path, &bytes)?;
        let byte_len = bytes.len() as u64;
        let cached_bytes = (byte_len <= IN_MEMORY_CACHE_MAX).then(|| Arc::new(bytes));
        self.snapshots.push(Snapshot {
            id,
            name: display_name,
            captured_at: jiff::Timestamp::now(),
            byte_len,
            sidecar_path,
            cached_bytes,
        });
        self.persist_index();
        Ok(id)
    }

    /// Rename the snapshot in place. No-op when the id is
    /// missing.
    pub fn rename(&mut self, id: SnapshotId, name: String) {
        if let Some(snap) = self.snapshots.iter_mut().find(|s| s.id == id) {
            snap.name = name;
            self.persist_index();
        }
    }

    /// Delete a snapshot and its sidecar bytes. Best-effort:
    /// missing sidecar files don't fail the call.
    pub fn delete(&mut self, id: SnapshotId) {
        let Some(idx) = self.snapshots.iter().position(|s| s.id == id) else { return };
        let removed = self.snapshots.remove(idx);
        if removed.sidecar_path.exists()
            && let Err(e) = std::fs::remove_file(&removed.sidecar_path)
        {
            tracing::warn!(error = %e, path = %removed.sidecar_path.display(), "delete snapshot sidecar");
        }
        self.persist_index();
    }

    /// Look up by id without mutating the store.
    pub fn get(&self, id: SnapshotId) -> Option<&Snapshot> {
        self.snapshots.iter().find(|s| s.id == id)
    }

    /// Number of bytes the in-memory cache currently holds.
    /// Surfaced in the snapshot panel header so the user can see
    /// the cost of the snapshots they've taken.
    pub fn cached_bytes(&self) -> u64 {
        self.snapshots.iter().filter_map(|s| s.cached_bytes.as_ref().map(|b| b.len() as u64)).sum()
    }

    fn persist_index(&self) {
        let Some(dir) = &self.sidecar_dir else { return };
        let index_path = dir.join("index.json");
        let entries: Vec<PersistedSnapshot> = self
            .snapshots
            .iter()
            .map(|s| PersistedSnapshot {
                id: s.id,
                name: s.name.clone(),
                captured_at: s.captured_at,
                byte_len: s.byte_len,
            })
            .collect();
        let payload = PersistedIndex { next_id: self.next_id, entries };
        let bytes = match serde_json::to_vec_pretty(&payload) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "serialize snapshot index");
                return;
            }
        };
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(error = %e, "create snapshot dir");
            return;
        }
        if let Err(e) = std::fs::write(&index_path, &bytes) {
            tracing::warn!(error = %e, path = %index_path.display(), "write snapshot index");
        }
    }
}

#[derive(Serialize, Deserialize)]
struct PersistedIndex {
    next_id: u64,
    entries: Vec<PersistedSnapshot>,
}

#[derive(Serialize, Deserialize)]
struct PersistedSnapshot {
    id: SnapshotId,
    name: String,
    captured_at: jiff::Timestamp,
    byte_len: u64,
}

/// Compute the per-file directory under
/// `$DATA_DIR/hxy/snapshots/` where `key_path`'s sidecar bytes
/// live. Returns `None` when the platform doesn't expose a
/// data dir, in which case captures fall back to a tempfile
/// sidecar that doesn't survive the session.
fn snapshot_dir_for(key_path: &Path) -> Option<PathBuf> {
    let base = dirs::data_dir()?.join(APP_NAME).join("snapshots");
    Some(base.join(snapshot_dir_name(key_path)))
}

fn snapshot_dir_name(key_path: &Path) -> String {
    let canonical = key_path.canonicalize().unwrap_or_else(|_| key_path.to_path_buf());
    let bytes = canonical.to_string_lossy().into_owned();
    let digest = HashAlgorithm::Blake3.compute(bytes.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in &digest {
        use std::fmt::Write;
        write!(&mut hex, "{b:02x}").expect("infallible");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_in(dir: &Path) -> SnapshotStore {
        SnapshotStore { next_id: 1, snapshots: Vec::new(), sidecar_dir: Some(dir.to_path_buf()) }
    }

    #[test]
    fn capture_creates_sidecar_and_caches_small_bytes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut store = store_in(tmp.path());
        let id = store.capture("first".into(), vec![1, 2, 3, 4]).unwrap();
        let snap = store.get(id).expect("snapshot exists");
        assert_eq!(snap.byte_len, 4);
        assert!(snap.sidecar_path.exists());
        assert!(snap.is_cached());
        let bytes = snap.load_bytes().unwrap();
        assert_eq!(bytes.as_slice(), &[1, 2, 3, 4]);
    }

    #[test]
    fn restore_round_trips_through_index() {
        let tmp = tempfile::TempDir::new().unwrap();
        {
            let mut store = store_in(tmp.path());
            let _ = store.capture("a".into(), vec![10, 20]).unwrap();
            let _ = store.capture("b".into(), vec![30]).unwrap();
        }
        // Re-open; the sidecar files are still in place so
        // restore() should find both entries.
        let mut store = SnapshotStore { next_id: 1, snapshots: Vec::new(), sidecar_dir: Some(tmp.path().to_path_buf()) };
        let bytes = std::fs::read(tmp.path().join("index.json")).unwrap();
        let parsed: PersistedIndex = serde_json::from_slice(&bytes).unwrap();
        store.next_id = parsed.next_id;
        for entry in parsed.entries {
            let sidecar_path = tmp.path().join(format!("{}.bin", entry.id.get()));
            store.snapshots.push(Snapshot {
                id: entry.id,
                name: entry.name,
                captured_at: entry.captured_at,
                byte_len: entry.byte_len,
                sidecar_path,
                cached_bytes: None,
            });
        }
        assert_eq!(store.snapshots.len(), 2);
        assert_eq!(store.snapshots[0].name, "a");
        assert_eq!(store.snapshots[1].name, "b");
        let b1 = store.snapshots[0].load_bytes().unwrap();
        assert_eq!(b1.as_slice(), &[10, 20]);
    }

    #[test]
    fn delete_removes_sidecar_and_entry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut store = store_in(tmp.path());
        let id = store.capture("x".into(), vec![1]).unwrap();
        let path = store.get(id).unwrap().sidecar_path.clone();
        assert!(path.exists());
        store.delete(id);
        assert!(!path.exists());
        assert!(store.get(id).is_none());
    }

    #[test]
    fn rename_updates_label() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut store = store_in(tmp.path());
        let id = store.capture("first".into(), vec![1]).unwrap();
        store.rename(id, "renamed".into());
        assert_eq!(store.get(id).unwrap().name, "renamed");
    }
}
