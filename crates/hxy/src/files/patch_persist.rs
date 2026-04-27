//! Sidecar storage for unsaved per-tab patches.
//!
//! When the app shuts down with dirty tabs, each one's patch is
//! serialised next to a snapshot of the source's filesystem
//! metadata under `$DATA_DIR/hxy/edits/`. On the next launch the
//! file open path checks for a matching sidecar; if the metadata
//! still matches what's on disk, the patch is offered back to the
//! user via a restore prompt.
//!
//! The sidecar stores absolute source paths so multiple tabs of
//! the same file collapse onto one entry; the file-name on disk is
//! a SHA-256 of the canonical path so it stays filesystem-safe and
//! deterministic across runs.

#![cfg(not(target_arch = "wasm32"))]

use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use suture::Patch;
use suture::metadata::FileMetadata;
use suture::metadata::HashAlgorithm;
use suture::metadata::SourceDigest;
use suture::metadata::SourceMetadata;

use crate::files::EditEntry;

/// Largest source we'll content-hash on quit. Above this, the
/// sidecar keeps only filesystem metadata -- hashing tens of MB/s
/// would stall shutdown on a GB-scale file. The integrity check on
/// restore falls back to (size, mtime) for these.
pub const DIGEST_MAX_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PatchSidecar {
    /// Absolute, canonicalised path of the source the patch was
    /// generated against. Cross-checked at restore time so a sidecar
    /// can't be misapplied when the user moves files around.
    pub source_path: PathBuf,
    pub metadata: SourceMetadata,
    pub patch: Patch,
    /// Undo history at snapshot time. Restored alongside the patch so
    /// the user can keep reaching backward through edits made in the
    /// previous session. `#[serde(default)]` keeps sidecars from
    /// older builds readable (they land with an empty history).
    #[serde(default)]
    pub undo_stack: Vec<EditEntry>,
    /// Redo history at snapshot time; same backward-compat treatment.
    #[serde(default)]
    pub redo_stack: Vec<EditEntry>,
}

/// How confident we are that the on-disk source still matches what
/// the sidecar was generated against. Drives the restore prompt:
/// `Clean` takes the short path, `Modified` shows a warning, and
/// `Unknown` lets the user decide without a definitive signal.
#[derive(Clone, Debug)]
pub enum RestoreIntegrity {
    /// Filesystem size + mtime match the sidecar snapshot.
    Clean,
    /// Something observable differs; `reason` is a short human-
    /// readable summary for the modal.
    Modified { reason: String },
    /// No comparable metadata -- the sidecar doesn't carry file
    /// stats, or we couldn't read the source's current metadata.
    Unknown { reason: String },
}

impl PatchSidecar {
    /// Classify the on-disk source against the sidecar's recorded
    /// metadata. Always returns -- the caller decides whether to
    /// prompt, warn, or bail based on the variant.
    pub fn integrity(&self) -> RestoreIntegrity {
        let Some(expected) = self.metadata.file else {
            return RestoreIntegrity::Unknown { reason: "no filesystem metadata recorded".into() };
        };
        let meta = match fs::metadata(&self.source_path) {
            Ok(m) => m,
            Err(e) => return RestoreIntegrity::Modified { reason: format!("source unreachable: {e}") },
        };
        let current = match FileMetadata::from_metadata(&meta) {
            Ok(c) => c,
            Err(e) => return RestoreIntegrity::Unknown { reason: format!("read file metadata: {e}") },
        };
        if current == expected {
            return RestoreIntegrity::Clean;
        }
        let mut diffs = Vec::new();
        if current.size != expected.size {
            diffs.push(format!("size {} -> {}", expected.size, current.size));
        }
        if (current.mtime_seconds, current.mtime_nanos) != (expected.mtime_seconds, expected.mtime_nanos) {
            diffs.push("modification time changed".into());
        }
        RestoreIntegrity::Modified { reason: diffs.join(", ") }
    }
}

/// Compute the on-disk filename for a sidecar covering `source_path`.
/// Hashing the canonical path keeps the name filesystem-safe and
/// stable across runs.
pub fn sidecar_filename(source_path: &Path) -> String {
    let canonical = source_path.canonicalize().unwrap_or_else(|_| source_path.to_path_buf());
    let bytes = canonical.to_string_lossy().into_owned();
    let digest = HashAlgorithm::Blake3.compute(bytes.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2 + 5);
    for b in &digest {
        use std::fmt::Write;
        write!(&mut hex, "{b:02x}").expect("infallible");
    }
    hex.push_str(".json");
    hex
}

pub fn sidecar_path(dir: &Path, source_path: &Path) -> PathBuf {
    dir.join(sidecar_filename(source_path))
}

pub fn store(dir: &Path, sidecar: &PatchSidecar) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = sidecar_path(dir, &sidecar.source_path);
    let json = serde_json::to_vec_pretty(sidecar).map_err(io::Error::other)?;
    fs::write(&path, json)?;
    Ok(path)
}

pub fn load(dir: &Path, source_path: &Path) -> io::Result<Option<PatchSidecar>> {
    let path = sidecar_path(dir, source_path);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let sidecar: PatchSidecar = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    Ok(Some(sidecar))
}

pub fn discard(dir: &Path, source_path: &Path) -> io::Result<()> {
    let path = sidecar_path(dir, source_path);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Snapshot a tab's patch into a [`PatchSidecar`]. Filesystem
/// metadata (size + mtime) is always recorded -- cheap and catches
/// the common case of the user editing the file out from under us.
/// A content digest is additionally computed for sources smaller
/// than [`DIGEST_MAX_BYTES`]; above that it'd stall shutdown.
/// Returns `None` for an empty patch (nothing worth persisting).
///
/// The undo / redo stacks are stored verbatim so the user can keep
/// stepping backward and forward through the previous session's
/// edits after the tab is reopened.
pub fn snapshot(
    source_path: PathBuf,
    source: &dyn hxy_core::HexSource,
    patch: Patch,
    undo_stack: Vec<EditEntry>,
    redo_stack: Vec<EditEntry>,
) -> Option<PatchSidecar> {
    if patch.is_empty() {
        return None;
    }
    let len = source.len().get();
    let mut metadata = SourceMetadata::new(len);
    if let Ok(meta) = fs::metadata(&source_path)
        && let Ok(file_meta) = FileMetadata::from_metadata(&meta)
    {
        metadata = metadata.with_file(file_meta);
    }
    if len <= DIGEST_MAX_BYTES
        && let Ok(bytes) = source.read(
            hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(len))
                .expect("range valid"),
        )
    {
        let digest = HashAlgorithm::Blake3.compute(&bytes);
        // BLAKE3 always produces 32 bytes; `SourceDigest::new` only
        // errors on length mismatch.
        let source_digest = SourceDigest::new(HashAlgorithm::Blake3, digest).expect("blake3 digest is 32 bytes");
        metadata = metadata.with_digest(source_digest);
    }
    Some(PatchSidecar { source_path, metadata, patch, undo_stack, redo_stack })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_sidecar() -> PatchSidecar {
        let mut patch = Patch::new();
        patch.write(0, vec![0xAA]).unwrap();
        PatchSidecar {
            source_path: PathBuf::from("/tmp/example.bin"),
            metadata: SourceMetadata::new(16),
            patch,
            undo_stack: vec![EditEntry { offset: 0, old_bytes: vec![0x00], new_bytes: vec![0xAA] }],
            redo_stack: vec![EditEntry { offset: 4, old_bytes: vec![0x22], new_bytes: vec![0xBB] }],
        }
    }

    #[test]
    fn sidecar_roundtrips_undo_and_redo_stacks() {
        let original = sample_sidecar();
        let json = serde_json::to_vec(&original).unwrap();
        let loaded: PatchSidecar = serde_json::from_slice(&json).unwrap();
        assert_eq!(loaded.undo_stack.len(), 1);
        assert_eq!(loaded.undo_stack[0].offset, 0);
        assert_eq!(loaded.undo_stack[0].new_bytes, vec![0xAA]);
        assert_eq!(loaded.redo_stack.len(), 1);
        assert_eq!(loaded.redo_stack[0].offset, 4);
    }

    #[test]
    fn legacy_sidecar_without_stacks_still_parses() {
        // Sidecars written by earlier builds have no undo_stack /
        // redo_stack field. `#[serde(default)]` makes them deserialise
        // with empty histories instead of erroring.
        let json = serde_json::json!({
            "source_path": "/tmp/example.bin",
            "metadata": {"len": 16},
            "patch": serde_json::to_value(&sample_sidecar().patch).unwrap(),
        });
        let loaded: PatchSidecar = serde_json::from_value(json).unwrap();
        assert!(loaded.undo_stack.is_empty());
        assert!(loaded.redo_stack.is_empty());
    }
}
