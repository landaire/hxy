//! Unsaved-patch sidecar wiring for the gpui shell: the on-quit
//! persistence pass and the reopen-time restore lookup.
//!
//! The sidecar store itself is framework-agnostic
//! ([`hxy_panels::files::patch_persist`]); this module only supplies the
//! gpui-suffixed edits directory and the glue to the workspace's file
//! tabs (the on-quit snapshot and the restore-prompt dialog live in
//! [`crate::workspace`]).

use std::path::Path;
use std::path::PathBuf;

use hxy_panels::files::patch_persist;
use hxy_panels::files::patch_persist::PatchSidecar;
use hxy_panels::files::patch_persist::RestoreIntegrity;

/// Gpui-suffixed unsaved-edits directory, deliberately separate from the
/// egui app's `$DATA_DIR/hxy/edits` so the two front ends never race on
/// the same sidecar index (the sidecar filenames hash the source path,
/// so a shared dir would otherwise collide). `None` when no platform
/// data dir resolves (persistence is then disabled, same as the layout).
pub(crate) fn edits_dir() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(dir) = EDITS_DIR_OVERRIDE.with(|cell| cell.borrow().clone()) {
        return Some(dir);
    }
    crate::persist::storage_dir().map(|dir| dir.join("gpui").join("edits"))
}

#[cfg(test)]
thread_local! {
    static EDITS_DIR_OVERRIDE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Redirect [`edits_dir`] to `path` for the current test thread, so sidecar
/// persistence and restore never touch the real data dir.
#[cfg(test)]
pub(crate) fn set_edits_dir_for_test(path: PathBuf) {
    EDITS_DIR_OVERRIDE.with(|cell| *cell.borrow_mut() = Some(path));
}

/// Load the sidecar covering `path`, if one exists, paired with its
/// on-disk integrity classification. `None` when there is no sidecar (the
/// common case) or the edits dir is unavailable.
pub(crate) fn load_restore(path: &Path) -> Option<(PatchSidecar, RestoreIntegrity)> {
    let dir = edits_dir()?;
    let sidecar = patch_persist::load(&dir, path).ok().flatten()?;
    let integrity = sidecar.integrity();
    Some((sidecar, integrity))
}

/// Drop any sidecar covering `path` (best-effort). Called after a save
/// (the buffer is now clean) and after a restore/discard decision.
pub(crate) fn discard(path: &Path) {
    if let Some(dir) = edits_dir() {
        let _ = patch_persist::discard(&dir, path);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hxy_core::HexSource;
    use hxy_core::MemorySource;
    use hxy_editor::HexEditor;

    use super::*;

    /// A sidecar snapshotted from a dirty buffer round-trips back through
    /// `load` with its patch and integrity intact -- the store/restore
    /// path the on-quit hook and the reopen prompt rely on.
    #[test]
    fn sidecar_store_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let source_path = dir.path().join("data.bin");
        std::fs::write(&source_path, [0u8; 8]).unwrap();

        let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(vec![0u8; 8]));
        let mut editor = HexEditor::new(source);
        editor.splice(0, 1, vec![0xAB]).unwrap();
        let patch = editor.patch().read().unwrap().clone();
        let sidecar =
            patch_persist::snapshot(source_path.clone(), editor.source().as_ref(), patch, Vec::new(), Vec::new())
                .expect("non-empty patch");
        patch_persist::store(dir.path(), &sidecar).unwrap();

        let loaded = patch_persist::load(dir.path(), &source_path).unwrap().expect("sidecar present");
        assert_eq!(loaded.patch.len(), 1);
        assert!(matches!(loaded.integrity(), RestoreIntegrity::Clean));
    }
}
