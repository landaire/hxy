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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PatchSidecar {
    /// Absolute, canonicalised path of the source the patch was
    /// generated against. Cross-checked at restore time so a sidecar
    /// can't be misapplied when the user moves files around.
    pub source_path: PathBuf,
    pub metadata: SourceMetadata,
    pub patch: Patch,
}

impl PatchSidecar {
    /// Verify the sidecar against the live filesystem state of
    /// `source_path`. The check is best-effort cheap: just
    /// `(size, mtime)`. The full content digest (if recorded)
    /// gets re-checked when the patch is actually applied.
    pub fn matches_disk(&self) -> bool {
        let Some(file_meta) = self.metadata.file else { return true };
        let Ok(meta) = fs::metadata(&self.source_path) else { return false };
        let Ok(current) = FileMetadata::from_metadata(&meta) else { return false };
        current == file_meta
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

/// Snapshot a tab's source bytes into a [`PatchSidecar`]. Reads
/// `source` once to compute the BLAKE3 digest so a moved-but-same-
/// content file still verifies on restore. Returns `None` if the
/// tab has no patch worth saving (empty patch).
pub fn snapshot(source_path: PathBuf, source: &dyn hxy_core::HexSource, patch: Patch) -> Option<PatchSidecar> {
    if patch.is_empty() {
        return None;
    }
    let len = source.len().get();
    let bytes = source
        .read(
            hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(len))
                .expect("range valid"),
        )
        .ok()?;
    let digest = HashAlgorithm::Blake3.compute(&bytes);
    // `SourceDigest::new` validates the digest length against the
    // algorithm; BLAKE3 always returns 32 bytes so this never fails
    // in practice -- unwrap tells a future reviewer exactly why.
    let source_digest = SourceDigest::new(HashAlgorithm::Blake3, digest).expect("blake3 digest is 32 bytes");
    let mut metadata = SourceMetadata::new(len).with_digest(source_digest);
    if let Ok(meta) = fs::metadata(&source_path)
        && let Ok(file_meta) = FileMetadata::from_metadata(&meta)
    {
        metadata = metadata.with_file(file_meta);
    }
    Some(PatchSidecar { source_path, metadata, patch })
}
