//! Modal dialog renders triggered from the per-frame loop. Each
//! dialog reads (and clears) a `pending_*` slot on `HxyApp` and
//! routes the user's choice back into the appropriate subsystem.

#![cfg(not(target_arch = "wasm32"))]

use crate::app::ConsoleSeverity;
use crate::app::ExternalChangeKind;
use crate::app::HxyApp;
use crate::app::ReloadDecision;
use crate::settings::AutoReloadMode;

enum DuplicateAction {
    Focus,
    OpenNewTab,
    Cancel,
}

enum RestoreAction {
    Restore,
    Discard,
}

enum ReloadAction {
    Reload(ReloadDecision),
    Cancel,
}

enum OrphanAction {
    Close,
    Keep,
}

/// Modal asking the user what to do when an open request collides
/// with an already-open tab pointing at the same path: focus the
/// existing tab, open a second copy, or cancel.
pub fn render_duplicate_open_dialog(ctx: &egui::Context, app: &mut HxyApp) {
    if app.pending_duplicate.is_none() {
        return;
    }
    let (name, path_display) = {
        let p = app.pending_duplicate.as_ref().unwrap();
        (p.display_name.clone(), p.path.display().to_string())
    };

    let mut action: Option<DuplicateAction> = None;
    let mut open = true;
    egui::Window::new(hxy_i18n::t("duplicate-open-title"))
        .id(egui::Id::new("hxy_duplicate_open_dialog"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(hxy_i18n::t("duplicate-open-body"));
            ui.add_space(4.0);
            ui.label(egui::RichText::new(&name).strong());
            ui.label(egui::RichText::new(&path_display).weak());
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(hxy_i18n::t("duplicate-open-focus")).clicked() {
                    action = Some(DuplicateAction::Focus);
                }
                if ui.button(hxy_i18n::t("duplicate-open-new-tab")).clicked() {
                    action = Some(DuplicateAction::OpenNewTab);
                }
                if ui.button(hxy_i18n::t("duplicate-open-cancel")).clicked() {
                    action = Some(DuplicateAction::Cancel);
                }
            });
        });

    if !open && action.is_none() {
        action = Some(DuplicateAction::Cancel);
    }

    let Some(action) = action else { return };
    let pending = app.pending_duplicate.take().unwrap();
    match action {
        DuplicateAction::Focus => {
            app.focus_file_tab(pending.existing);
        }
        DuplicateAction::OpenNewTab => {
            if let Err(e) = app.open_filesystem_path(pending.display_name, pending.path, None, None) {
                tracing::warn!(error = %e, "open duplicate tab");
            }
        }
        DuplicateAction::Cancel => {}
    }
}

/// First-launch prompt asking the user whether to download the
/// upstream ImHex-Patterns corpus. Renders a modal Window once;
/// the user picks Download (kicks off the worker), Not Now (the
/// dialog disappears for this session, returns next launch), or
/// Don't Ask Again (persists the decline so settings becomes the
/// only path forward).
pub fn render_imhex_patterns_first_run(ctx: &egui::Context, app: &mut HxyApp) {
    if !app.pattern_first_run_prompt {
        return;
    }
    let mut close = false;
    let mut start_download = false;
    let mut decline_permanent = false;
    egui::Window::new(hxy_i18n::t("patterns-prompt-title"))
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            ui.set_max_width(420.0);
            ui.label(hxy_i18n::t("patterns-prompt-body"));
            ui.add_space(8.0);
            ui.colored_label(egui::Color32::from_rgb(220, 180, 0), hxy_i18n::t("patterns-prompt-disclaimer"));
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button(hxy_i18n::t("patterns-prompt-download")).clicked() {
                    start_download = true;
                    close = true;
                }
                if ui.button(hxy_i18n::t("patterns-prompt-not-now")).clicked() {
                    close = true;
                }
                if ui.button(hxy_i18n::t("patterns-prompt-dont-ask")).clicked() {
                    decline_permanent = true;
                    close = true;
                }
            });
        });
    if start_download {
        kick_off_pattern_download(ctx, app);
    }
    if decline_permanent {
        let mut g = app.state.write();
        g.app.imhex_patterns.declined_prompt = true;
    }
    if close {
        app.pattern_first_run_prompt = false;
    }
}

/// Spin up a background download (or no-op if one is already in
/// flight). Surfaces a toast on either start or hard failure.
pub fn kick_off_pattern_download(ctx: &egui::Context, app: &mut HxyApp) {
    if app.pattern_fetch.as_ref().is_some_and(|h| !h.is_done()) {
        return;
    }
    let Some(handle) = crate::templates::patterns_fetch::spawn_default_fetch(ctx) else {
        app.toasts.error(&hxy_i18n::t("patterns-fetch-no-data-dir"));
        return;
    };
    app.toasts.info(&hxy_i18n::t("patterns-fetch-started"));
    app.pattern_fetch = Some(handle);
}

/// Per-frame: pump any in-flight pattern fetch worker, surface its
/// status via toasts, and once it lands stash the SHA-256 + refresh
/// the template library so the new patterns appear immediately.
pub fn pump_pattern_fetch(ctx: &egui::Context, app: &mut HxyApp) {
    if std::mem::take(&mut app.pending_pattern_download_request) {
        kick_off_pattern_download(ctx, app);
    }
    let Some(handle) = app.pattern_fetch.as_mut() else {
        app.pattern_in_flight_bytes = None;
        return;
    };
    let snapshot = handle.pump(ctx).cloned();
    let Some(status) = snapshot else { return };
    if let crate::templates::patterns_fetch::FetchStatus::Progress { downloaded, .. } = &status {
        app.pattern_in_flight_bytes = Some(*downloaded);
    }
    if !handle.is_done() {
        return;
    }
    app.pattern_in_flight_bytes = None;
    app.pattern_fetch = None;
    match status {
        crate::templates::patterns_fetch::FetchStatus::Success { sha256_hex, extracted_root: _ } => {
            {
                let mut g = app.state.write();
                g.app.imhex_patterns.installed_hash = Some(sha256_hex);
                g.app.imhex_patterns.last_check = Some(jiff::Timestamp::now());
            }
            app.refresh_templates_after_pattern_install();
            app.toasts.success(&hxy_i18n::t("patterns-fetch-done"));
        }
        crate::templates::patterns_fetch::FetchStatus::Failed { message } => {
            let label = hxy_i18n::t_args("patterns-fetch-failed", &[("error", &message)]);
            app.toasts.error(&label);
        }
        crate::templates::patterns_fetch::FetchStatus::Progress { .. } => {
            // Pump only matches on terminal states; keep an explicit arm
            // so the match stays exhaustive without `_ => {}` swallowing
            // future additions.
        }
    }
}

/// Modal asking the user what to do with a workspace-entry tab
/// whose backing VFS path no longer resolves after a reload.
/// Renders one entry at a time; "Close" drops the tab through
/// the regular close path, "Keep open" leaves the editor's
/// last-known bytes available for inspection (writeback is
/// already broken, so any save attempt will surface its own
/// error).
pub fn render_orphaned_entry_dialog(ctx: &egui::Context, app: &mut HxyApp) {
    let Some(orphan) = app.pending_orphan_entries.first() else {
        return;
    };
    let display_name = orphan.display_name.clone();
    let entry_path = orphan.entry_path.clone();
    let file_id = orphan.file_id;

    let mut action: Option<OrphanAction> = None;
    let mut open = true;
    egui::Window::new(hxy_i18n::t("orphan-entry-title"))
        .id(egui::Id::new("hxy_orphan_entry_dialog"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .open(&mut open)
        .show(ctx, |ui| {
            ui.set_max_width(440.0);
            ui.label(hxy_i18n::t_args("orphan-entry-body", &[("name", &display_name), ("path", &entry_path)]));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .button(hxy_i18n::t("orphan-entry-close"))
                    .on_hover_text(hxy_i18n::t("orphan-entry-close-tooltip"))
                    .clicked()
                {
                    action = Some(OrphanAction::Close);
                }
                if ui
                    .button(hxy_i18n::t("orphan-entry-keep"))
                    .on_hover_text(hxy_i18n::t("orphan-entry-keep-tooltip"))
                    .clicked()
                {
                    action = Some(OrphanAction::Keep);
                }
            });
        });
    if !open && action.is_none() {
        action = Some(OrphanAction::Keep);
    }
    let Some(action) = action else { return };
    app.pending_orphan_entries.remove(0);
    match action {
        OrphanAction::Close => crate::tabs::close::close_file_tab_by_id(app, file_id),
        OrphanAction::Keep => {}
    }
}

/// Modal asking the user how to react to a watched file changing
/// on disk. Three terminal choices (Reload / Keep edits / Ignore)
/// plus an "Always do this for this file" toggle that writes the
/// per-file override into [`crate::settings::AppSettings::file_watch_prefs`].
pub fn render_reload_prompt_dialog(ctx: &egui::Context, app: &mut HxyApp) {
    if app.pending_reload_prompt.is_none() {
        return;
    }
    let (display_name, path_display, kind, has_unsaved) = {
        let p = app.pending_reload_prompt.as_ref().unwrap();
        (p.display_name.clone(), p.path.display().to_string(), p.kind, p.has_unsaved)
    };

    // The "always do this for this file" checkbox carries the
    // user's decision into a per-file pref; survives across the
    // dialog only via this local mutable. Default off.
    let id_for_pref = egui::Id::new("hxy_reload_prompt_remember");
    let mut remember = ctx.data_mut(|d| d.get_temp::<bool>(id_for_pref).unwrap_or(false));

    let mut action: Option<ReloadAction> = None;
    let mut open = true;
    egui::Window::new(hxy_i18n::t("reload-prompt-title"))
        .id(egui::Id::new("hxy_reload_prompt_dialog"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .open(&mut open)
        .show(ctx, |ui| {
            ui.set_max_width(440.0);
            let body_key = match kind {
                ExternalChangeKind::Modified => "reload-prompt-body-modified",
                ExternalChangeKind::Removed => "reload-prompt-body-removed",
            };
            ui.label(hxy_i18n::t_args(body_key, &[("name", &display_name)]));
            ui.label(egui::RichText::new(&path_display).weak());

            if has_unsaved {
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(hxy_i18n::t("reload-prompt-warn-unsaved"))
                        .color(ui.visuals().warn_fg_color)
                        .strong(),
                );
            }

            ui.add_space(8.0);
            ui.checkbox(&mut remember, hxy_i18n::t("reload-prompt-remember"));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .button(hxy_i18n::t("reload-prompt-discard"))
                    .on_hover_text(hxy_i18n::t("reload-prompt-discard-tooltip"))
                    .clicked()
                {
                    action = Some(ReloadAction::Reload(ReloadDecision::DiscardEdits));
                }
                if ui
                    .button(hxy_i18n::t("reload-prompt-keep"))
                    .on_hover_text(hxy_i18n::t("reload-prompt-keep-tooltip"))
                    .clicked()
                {
                    action = Some(ReloadAction::Reload(ReloadDecision::KeepEdits));
                }
                if ui
                    .button(hxy_i18n::t("reload-prompt-ignore"))
                    .on_hover_text(hxy_i18n::t("reload-prompt-ignore-tooltip"))
                    .clicked()
                {
                    action = Some(ReloadAction::Reload(ReloadDecision::Ignore));
                }
                if ui.button(hxy_i18n::t("reload-prompt-cancel")).clicked() {
                    action = Some(ReloadAction::Cancel);
                }
            });
        });
    ctx.data_mut(|d| d.insert_temp(id_for_pref, remember));

    if !open && action.is_none() {
        action = Some(ReloadAction::Cancel);
    }
    let Some(action) = action else { return };

    let pending = app.pending_reload_prompt.take().expect("checked above");
    ctx.data_mut(|d| d.remove::<bool>(id_for_pref));

    let decision = match action {
        ReloadAction::Reload(d) => d,
        ReloadAction::Cancel => return,
    };

    if remember {
        let mode = match decision {
            ReloadDecision::DiscardEdits => Some(AutoReloadMode::Always),
            ReloadDecision::Ignore => Some(AutoReloadMode::Never),
            // "Keep edits" doesn't map to a global setting --
            // reset to ask, so the user is prompted again next
            // time and isn't silently locked into one branch.
            ReloadDecision::KeepEdits => None,
        };
        match mode {
            Some(m) => {
                // Route through the watcher-aware setter so a
                // Never decision actually unwatches and a
                // switch back to Always re-enrols.
                app.set_file_watch_pref(pending.file_id, m);
            }
            None => {
                let mut g = app.state.write();
                g.app.set_auto_reload_for(pending.path.clone(), None);
            }
        }
    }
    if matches!(decision, ReloadDecision::Ignore) {
        // Bump the watcher's snapshot so it doesn't immediately
        // re-fire on the same change.
        if let Some(watcher) = app.file_watcher.as_mut() {
            watcher.mark_synced(&pending.path);
        }
        app.console_log(
            ConsoleSeverity::Info,
            format!("Reload {}", pending.path.display()),
            "ignored disk change; in-memory bytes unchanged",
        );
        return;
    }
    app.apply_reload_decision(ctx, pending.file_id, decision);
}

/// Modal asking the user whether to restore an unsaved-edits sidecar
/// found on open. Clean sidecars get the short path; modified /
/// unknown ones get a yellow warning banner plus a worded
/// "restore anyway" button so the risk isn't accidental.
pub fn render_patch_restore_dialog(ctx: &egui::Context, app: &mut HxyApp) {
    use crate::files::patch_persist::RestoreIntegrity;

    if app.pending_patch_restore.is_none() {
        return;
    }
    let (path_display, op_count, integrity) = {
        let p = app.pending_patch_restore.as_ref().unwrap();
        (p.sidecar.source_path.display().to_string(), p.sidecar.patch.len(), p.integrity.clone())
    };

    let mut action: Option<RestoreAction> = None;
    let mut open = true;
    egui::Window::new(hxy_i18n::t("restore-patch-title"))
        .id(egui::Id::new("hxy_restore_patch_dialog"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(hxy_i18n::t_args("restore-patch-body", &[("ops", &op_count.to_string())]));
            ui.label(egui::RichText::new(&path_display).weak());

            match &integrity {
                RestoreIntegrity::Clean => {}
                RestoreIntegrity::Modified { reason } => {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(hxy_i18n::t("restore-patch-warn-modified"))
                            .color(ui.visuals().warn_fg_color)
                            .strong(),
                    );
                    ui.label(egui::RichText::new(reason).weak());
                }
                RestoreIntegrity::Unknown { reason } => {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(hxy_i18n::t("restore-patch-warn-unknown"))
                            .color(ui.visuals().warn_fg_color)
                            .strong(),
                    );
                    ui.label(egui::RichText::new(reason).weak());
                }
            }

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let restore_label = match &integrity {
                    RestoreIntegrity::Clean => hxy_i18n::t("restore-patch-restore"),
                    _ => hxy_i18n::t("restore-patch-restore-anyway"),
                };
                if ui.button(restore_label).clicked() {
                    action = Some(RestoreAction::Restore);
                }
                if ui.button(hxy_i18n::t("restore-patch-discard")).clicked() {
                    action = Some(RestoreAction::Discard);
                }
            });
        });
    if !open {
        app.pending_patch_restore = None;
        return;
    }
    let Some(action) = action else { return };

    let pending = app.pending_patch_restore.take().unwrap();
    let path = pending.sidecar.source_path.clone();
    let dir = crate::files::save::unsaved_edits_dir();
    match action {
        RestoreAction::Restore => {
            let ctx_label = format!("Restore {}", path.display());
            let integrity_clean = matches!(pending.integrity, RestoreIntegrity::Clean);
            let mut log_lines: Vec<(ConsoleSeverity, String)> = Vec::new();
            let accept = if let Some(file) = app.files.get_mut(&pending.file_id) {
                let verified = if integrity_clean {
                    let len = file.editor.source().len().get();
                    match file.editor.source().read(
                        hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(len))
                            .expect("range valid"),
                    ) {
                        Ok(bytes) => match pending.sidecar.metadata.verify(&bytes) {
                            Ok(()) => true,
                            Err(e) => {
                                log_lines.push((
                                    ConsoleSeverity::Warning,
                                    format!("source verification failed; not restoring: {e}"),
                                ));
                                false
                            }
                        },
                        Err(e) => {
                            log_lines.push((ConsoleSeverity::Error, format!("re-read source: {e}")));
                            false
                        }
                    }
                } else {
                    true
                };

                if verified {
                    *file.editor.patch().write().expect("patch lock poisoned") = pending.sidecar.patch;
                    file.editor.set_undo_stack(pending.sidecar.undo_stack);
                    file.editor.set_redo_stack(pending.sidecar.redo_stack);
                    file.editor.push_history_boundary();
                    file.editor.set_edit_mode(crate::files::EditMode::Mutable);
                    if integrity_clean {
                        log_lines.push((ConsoleSeverity::Info, "restored unsaved edits".to_owned()));
                    } else {
                        log_lines.push((
                            ConsoleSeverity::Warning,
                            "restored unsaved edits onto a file whose on-disk state has changed".to_owned(),
                        ));
                    }
                }
                verified
            } else {
                false
            };
            let _ = accept;
            for (severity, message) in log_lines {
                app.console_log(severity, &ctx_label, message);
            }
            if let Some(dir) = dir {
                let _ = crate::files::patch_persist::discard(&dir, &path);
            }
        }
        RestoreAction::Discard => {
            if let Some(dir) = dir {
                let _ = crate::files::patch_persist::discard(&dir, &path);
            }
        }
    }
}
