//! File save flow: dispatch to filesystem write or VFS-writer
//! poke depending on the tab's source kind, plus the atomic
//! write helper and the persisted-edits sidecar location.

#![cfg(not(target_arch = "wasm32"))]

use hxy_vfs::TabSource;

use crate::APP_NAME;
use crate::app::ConsoleSeverity;
use crate::app::HxyApp;
use crate::files::FileId;

/// Save the active tab. `force_dialog` always asks for a destination
/// (Save As); otherwise the tab's existing filesystem path is used
/// when present, falling back to the dialog when there isn't one.
pub fn save_active_file(app: &mut HxyApp, force_dialog: bool) {
    let Some(id) = crate::app::active_file_id(app) else { return };
    let _ = save_file_by_id(app, id, force_dialog);
}

/// Save a specific file tab by id. Returns `true` when the bytes
/// actually hit disk; `false` when the user dismissed the dialog or
/// the write itself failed (the latter is also surfaced via the
/// console log). Used by [`save_active_file`] for the Save / Save
/// As shortcut path and by the close-tab-with-unsaved-changes
/// modal, which conditions the tab close on the save succeeding.
pub fn save_file_by_id(app: &mut HxyApp, id: FileId, force_dialog: bool) -> bool {
    let Some(file) = app.files.get(&id) else { return false };
    // VFS-entry tabs (e.g. xbox-neighborhood `/memory/<addr>`)
    // have no filesystem path -- the save flow walks each patch
    // op back through the parent mount's `VfsWriter` instead.
    // `force_dialog` (Save As) still falls through to the
    // filesystem path so the user can spill VFS bytes to disk.
    if !force_dialog && let Some(TabSource::VfsEntry { .. }) = file.source_kind.as_ref() {
        return save_vfs_entry_in_place(app, id);
    }
    let display = file.display_name.clone();
    let target = if force_dialog {
        let mut dialog = rfd::FileDialog::new().set_file_name(&display);
        if let Some(parent) = file.root_path().and_then(|p| p.parent()) {
            dialog = dialog.set_directory(parent);
        }
        dialog.save_file()
    } else {
        match file.root_path().cloned() {
            Some(p) => Some(p),
            None => rfd::FileDialog::new().set_file_name(&display).save_file(),
        }
    };
    let Some(path) = target else { return false };

    let ctx = format!("Save {}", path.display());
    let len = file.editor.source().len().get();
    let bytes = match file.editor.source().read(
        hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(len)).expect("valid range"),
    ) {
        Ok(b) => b,
        Err(e) => {
            app.console_log(ConsoleSeverity::Error, &ctx, format!("read patched bytes: {e}"));
            return false;
        }
    };
    if let Err(e) = write_atomic(&path, &bytes) {
        app.console_log(ConsoleSeverity::Error, &ctx, format!("write: {e}"));
        return false;
    }

    let previous_source = app.files.get(&id).and_then(|f| f.source_kind.clone());
    let previous_root = app.files.get(&id).and_then(|f| f.root_path().cloned());
    // After atomic-write the in-memory `bytes` Vec is no longer
    // needed; swap the editor onto a streaming source over the
    // just-written file so a freshly-saved 4 GiB blob doesn't
    // stay resident as a `MemorySource`. Falls back to the
    // in-memory wrapping path if reopening for streaming
    // somehow fails (file deleted between rename and reopen, FD
    // limit, ...).
    let post_save_source: std::sync::Arc<dyn hxy_core::HexSource> =
        match crate::files::streaming::open_filesystem(&path) {
            Ok((s, _)) => {
                drop(bytes);
                s
            }
            Err(e) => {
                tracing::debug!(error = %e, path = %path.display(), "post-save streaming reopen failed; staying in-memory");
                std::sync::Arc::new(hxy_core::MemorySource::new(bytes))
            }
        };
    if let Some(file) = app.files.get_mut(&id) {
        file.editor.swap_source(post_save_source);
        file.source_kind = Some(hxy_vfs::TabSource::Filesystem(path.clone()));
        if let Some(name) = path.file_name() {
            file.display_name = name.to_string_lossy().into_owned();
        }
    }
    // The just-saved bytes are now what's on disk -- bump the
    // watcher's snapshot so the post-save mtime change doesn't
    // boomerang back as a phantom external-change prompt. If
    // Save As moved the tab to a new path, also re-aim the
    // watcher there and unwatch the previous root if no other
    // tab is still using it.
    if let Some(prev) = previous_root.as_ref()
        && prev != &path
    {
        app.unwatch_path_if_unused(prev);
    }
    app.watch_root_for_file(id);
    if let Some(watcher) = app.file_watcher.as_mut() {
        watcher.mark_synced(&path);
    }
    if let Some(dir) = unsaved_edits_dir() {
        let _ = crate::files::patch_persist::discard(&dir, &path);
    }
    if let Some(TabSource::Anonymous { id: anon_id, .. }) = previous_source.as_ref() {
        if let Some(anon_path) = crate::files::new::anonymous_file_path(*anon_id) {
            let _ = std::fs::remove_file(&anon_path);
        }
        let new_source = TabSource::Filesystem(path.clone());
        let mut state = app.state.write();
        state.open_tabs.retain(|t| !matches!(&t.source, TabSource::Anonymous { id, .. } if id == anon_id));
        if !state.open_tabs.iter().any(|t| t.source == new_source) {
            state.open_tabs.push(crate::state::OpenTabState {
                source: new_source,
                selection: None,
                scroll_offset: 0.0,
                as_workspace: false,
            });
        }
    }
    app.console_log(ConsoleSeverity::Info, &ctx, "saved");
    true
}

/// In-place writeback for a VFS-backed tab. Walks the editor's
/// patch ops and pushes each in-place write through the mount's
/// `VfsWriter`. Pure inserts / deletes are rejected because the
/// only writeback target today is xbox-neighborhood's `/memory/`
/// + `/modules/` namespaces, neither of which can grow or shrink.
///
/// On success the patch is cleared (the mount is now the source
/// of truth) and the file's byte source is swapped to the post-
/// write contents so the editor doesn't keep showing the patch
/// overlay against stale base bytes.
pub fn save_vfs_entry_in_place(app: &mut HxyApp, id: FileId) -> bool {
    let Some(file) = app.files.get(&id) else { return false };
    let TabSource::VfsEntry { parent, entry_path } = file.source_kind.as_ref().expect("checked") else {
        return false;
    };
    let entry_path = entry_path.clone();
    let parent_source = (**parent).clone();
    let display = file.display_name.clone();
    let ctx = format!("Save {display}");

    let mount = match app.find_mount_for_source(&parent_source) {
        Some(m) => m,
        None => {
            app.console_log(ConsoleSeverity::Error, &ctx, "parent VFS tab is gone -- close + reopen this tab");
            return false;
        }
    };
    let writer = match mount.writer.clone() {
        Some(w) => w,
        None => {
            app.console_log(ConsoleSeverity::Error, &ctx, "this VFS handler doesn't support writeback");
            return false;
        }
    };

    let (patch_ops, total_len, post_write_bytes) = {
        let file = app.files.get(&id).expect("just looked up");
        let editor = &file.editor;
        let total_len = editor.source().len().get();
        let patch = editor.patch().read().unwrap();
        let mut ops: Vec<(u64, Vec<u8>)> = Vec::with_capacity(patch.ops().len());
        let mut bad_op: Option<(u64, u64, usize)> = None;
        for op in patch.ops() {
            if op.old_len != op.new_bytes.len() as u64 {
                bad_op = Some((op.offset, op.old_len, op.new_bytes.len()));
                break;
            }
            ops.push((op.offset, op.new_bytes.clone()));
        }
        drop(patch);
        if let Some((offset, old_len, new_len)) = bad_op {
            app.console_log(
                ConsoleSeverity::Error,
                &ctx,
                format!(
                    "can't poke insert/delete (offset={offset}, old_len={old_len}, new_len={new_len}); only in-place writes"
                ),
            );
            return false;
        }
        let bytes = editor.source().read(
            hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(total_len))
                .expect("valid range"),
        );
        let bytes_result = bytes.map_err(|e| e.to_string());
        (ops, total_len, bytes_result)
    };

    if patch_ops.is_empty() {
        app.console_log(ConsoleSeverity::Info, &ctx, "no changes to save");
        return true;
    }

    let mut total_written: u64 = 0;
    let mut total_requested: u64 = 0;
    for (offset, bytes) in &patch_ops {
        let n = bytes.len() as u64;
        total_requested += n;
        match writer.write_range(&entry_path, *offset, bytes) {
            Ok(written) => {
                total_written += written;
                if written < n {
                    app.console_log(
                        ConsoleSeverity::Warning,
                        &ctx,
                        format!("partial write at offset {offset}: requested {n}, wrote {written}"),
                    );
                }
            }
            Err(e) => {
                app.console_log(ConsoleSeverity::Error, &ctx, format!("write @ offset {offset}: {e}"));
                return false;
            }
        }
    }

    match post_write_bytes {
        Ok(bytes) => {
            if let Some(file) = app.files.get_mut(&id) {
                let base: std::sync::Arc<dyn hxy_core::HexSource> =
                    std::sync::Arc::new(hxy_core::MemorySource::new(bytes));
                file.editor.swap_source(base);
            }
        }
        Err(e) => {
            app.console_log(
                ConsoleSeverity::Warning,
                &ctx,
                format!("post-write source rebuild: {e} (patch overlay still present)"),
            );
        }
    }

    let _ = total_len;
    app.console_log(
        ConsoleSeverity::Info,
        &ctx,
        format!("wrote {total_written}/{total_requested} bytes via plugin writer"),
    );
    true
}

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

/// Per-install storage for in-progress patches. Mirrors the file's
/// disk path under here so a reopen on the next launch can offer
/// to restore the unsaved edits without keeping the live editor
/// alive across runs.
pub fn unsaved_edits_dir() -> Option<std::path::PathBuf> {
    let base = dirs::data_dir()?;
    Some(base.join(APP_NAME).join("edits"))
}
