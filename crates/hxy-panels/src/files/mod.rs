//! Framework-agnostic file-persistence cores shared between hxy's
//! egui frontend and the GPUI port: the atomic-write helper, the
//! unsaved-patch sidecar store, and the per-file snapshot store.
//!
//! Nothing here reaches for a UI toolkit or either frontend's tab /
//! file model. Data-directory roots are passed in by the caller so
//! the two frontends can keep separate on-disk namespaces (see the
//! snapshot store's gpui-suffixed subdir) without this layer knowing
//! about either app's name.

#![cfg(not(target_arch = "wasm32"))]

pub mod patch_persist;
pub mod snapshot;

/// Write `bytes` to `path` atomically: stage in a sibling tempfile,
/// fsync, then rename. Avoids leaving a half-written file if the
/// process crashes mid-write.
pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.as_file_mut().write_all(bytes)?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::write_atomic;

    #[test]
    fn write_atomic_replaces_contents() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("out.bin");
        write_atomic(&path, &[1, 2, 3]).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), vec![1, 2, 3]);
        write_atomic(&path, &[9, 9]).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), vec![9, 9]);
    }
}
