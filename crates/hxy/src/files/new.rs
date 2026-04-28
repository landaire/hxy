//! New ("Untitled") file flow plus the on-disk sidecar location
//! that backs anonymous tabs across restarts.

#![cfg(not(target_arch = "wasm32"))]

use hxy_vfs::TabSource;

use crate::APP_NAME;
use crate::app::HxyApp;

/// Create a fresh anonymous ("Untitled") tab with a small zero-filled
/// buffer. Picks the next free `AnonymousId` and a "Untitled N" title
/// that doesn't collide with any already-open or persisted tab.
pub fn handle_new_file(app: &mut HxyApp) {
    let mut used_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for file in app.files.values() {
        if let Some(TabSource::Anonymous { id, .. }) = &file.source_kind {
            used_ids.insert(id.get());
        }
    }
    for tab in &app.state.read().open_tabs {
        if let TabSource::Anonymous { id, .. } = &tab.source {
            used_ids.insert(id.get());
        }
    }
    let next_id = (0u64..).find(|i| !used_ids.contains(i)).expect("u64 id space");

    let mut used_titles: std::collections::HashSet<String> = std::collections::HashSet::new();
    for file in app.files.values() {
        used_titles.insert(file.display_name.clone());
    }
    let title = (1u64..)
        .map(|n| if n == 1 { "Untitled".to_owned() } else { format!("Untitled {n}") })
        .find(|t| !used_titles.contains(t))
        .expect("nonzero range of titles");

    let id = hxy_vfs::AnonymousId(next_id);
    let bytes: Vec<u8> = Vec::new();
    if let Some(path) = anonymous_file_path(id) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&path, &bytes) {
            tracing::warn!(error = %e, path = %path.display(), "write anonymous tab seed");
        }
    }
    let source = TabSource::Anonymous { id, title: title.clone() };
    let initial_caret = Some(hxy_core::Selection::caret(hxy_core::ByteOffset::new(0)));
    let in_memory: std::sync::Arc<dyn hxy_core::HexSource> = std::sync::Arc::new(hxy_core::MemorySource::new(bytes));
    let file_id = app.open(title, Some(source), in_memory, initial_caret, None, false);
    app.focus_file_tab(file_id);
}

/// Per-install storage for anonymous / scratch tabs. One file per
/// tab named after the [`hxy_vfs::AnonymousId`], created on first
/// `New file` and removed when the tab is saved to a real path or
/// closed without saving.
pub fn anonymous_files_dir() -> Option<std::path::PathBuf> {
    let base = dirs::data_dir()?;
    Some(base.join(APP_NAME).join("anonymous"))
}

pub fn anonymous_file_path(id: hxy_vfs::AnonymousId) -> Option<std::path::PathBuf> {
    anonymous_files_dir().map(|d| d.join(format!("{:016x}.bin", id.get())))
}
