//! Compare picker modal + the palette / dialog entry points that
//! open it. Both flows resolve a pair of byte sources into a fresh
//! [`crate::compare::CompareSession`] and push a `Tab::Compare`.

#![cfg(not(target_arch = "wasm32"))]

use hxy_vfs::TabSource;

use crate::app::HxyApp;
use crate::files::FileId;
use crate::tabs::Tab;

/// In-progress state for the compare-picker modal -- which side is
/// currently selected. Lives on [`HxyApp::compare_picker`] until the
/// user confirms or cancels.
#[derive(Clone, Debug)]
pub struct ComparePickerState {
    pub a: ComparePickerSource,
    pub b: ComparePickerSource,
}

/// One side of the picker. `OpenFile` reuses the bytes from a
/// currently-open tab; `Filesystem` reads from disk on confirm.
/// `Unset` is the initial placeholder until the user picks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComparePickerSource {
    Unset,
    OpenFile(FileId),
    Filesystem(std::path::PathBuf),
}

#[derive(Debug, thiserror::Error)]
pub enum CompareSpawnError {
    #[error("source not selected")]
    Unset,
    #[error("read filesystem source {path}")]
    ReadFile {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("read open file bytes: {0}")]
    ReadOpenFile(String),
    #[error("open file {0} no longer exists")]
    OpenFileGone(u64),
}

/// Read-result for one side of the picker.
struct PickedSide {
    name: String,
    source: Option<TabSource>,
    bytes: Vec<u8>,
}

/// Open the picker dialog with A pre-filled to whatever tab is
/// focused (when its bytes have a stable source we can re-read).
pub fn start_compare_picker(app: &mut HxyApp) {
    app.palette.close();
    let auto_a = match crate::app::active_file_id(app).and_then(|id| app.files.get(&id).map(|f| (id, f))) {
        Some((id, f)) if f.source_kind.is_some() => ComparePickerSource::OpenFile(id),
        _ => ComparePickerSource::Unset,
    };
    app.compare_picker = Some(ComparePickerState { a: auto_a, b: ComparePickerSource::Unset });
}

/// Open the palette into the compare cascade, auto-selecting the
/// focused tab as A and jumping straight to the B-pick when
/// possible. Falls back to the A-pick mode if there's no active
/// file or the focused buffer is anonymous (no stable source).
pub fn start_compare_palette_flow(app: &mut HxyApp) {
    use crate::commands::palette::ComparePickState;
    use crate::commands::palette::Mode;
    let auto_a =
        crate::app::active_file_id(app).and_then(|id| app.files.get(&id)).and_then(|f| f.source_kind.clone());
    match auto_a {
        Some(source) => {
            app.palette.compare_pick = Some(ComparePickState { picked_a: Some(source) });
            app.palette.open_at(Mode::CompareSideB);
        }
        None => app.palette.open_at(Mode::CompareSideA),
    }
}

/// Render the compare picker modal, if open. Confirm spawns a
/// `Tab::Compare`; Cancel clears the slot.
pub fn render_compare_picker(ctx: &egui::Context, app: &mut HxyApp) {
    let Some(mut state) = app.compare_picker.take() else { return };
    let mut keep_open = true;
    let mut confirm = false;
    egui::Window::new(hxy_i18n::t("compare-picker-title"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.label(hxy_i18n::t("compare-picker-body"));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(hxy_i18n::t("compare-picker-side-a"));
                render_compare_picker_combo(ui, app, &mut state.a, "a");
            });
            ui.horizontal(|ui| {
                ui.label(hxy_i18n::t("compare-picker-side-b"));
                render_compare_picker_combo(ui, app, &mut state.b, "b");
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let ready =
                    !matches!(state.a, ComparePickerSource::Unset) && !matches!(state.b, ComparePickerSource::Unset);
                if ui.add_enabled(ready, egui::Button::new(hxy_i18n::t("compare-picker-confirm"))).clicked() {
                    confirm = true;
                    keep_open = false;
                }
                if ui.button(hxy_i18n::t("compare-picker-cancel")).clicked() {
                    keep_open = false;
                }
            });
        });
    if confirm && let Err(e) = spawn_compare_from_picker(app, ctx, &state) {
        tracing::warn!(error = %e, "spawn compare");
    }
    if keep_open {
        app.compare_picker = Some(state);
    }
}

fn render_compare_picker_combo(
    ui: &mut egui::Ui,
    app: &HxyApp,
    selection: &mut ComparePickerSource,
    salt: &'static str,
) {
    let label = match selection {
        ComparePickerSource::Unset => hxy_i18n::t("compare-picker-unset"),
        ComparePickerSource::OpenFile(id) => {
            app.files.get(id).map(|f| f.display_name.clone()).unwrap_or_else(|| format!("file-{}", id.get()))
        }
        ComparePickerSource::Filesystem(p) => {
            p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
        }
    };
    egui::ComboBox::from_id_salt(("hxy-compare-picker", salt)).selected_text(label).show_ui(ui, |ui| {
        if !app.files.is_empty() {
            ui.weak(hxy_i18n::t("compare-picker-section-open"));
            for (id, f) in &app.files {
                ui.selectable_value(selection, ComparePickerSource::OpenFile(*id), &f.display_name);
            }
        }
        let recent = app.state.read().app.recent_files.clone();
        if !recent.is_empty() {
            ui.separator();
            ui.weak(hxy_i18n::t("compare-picker-section-recent"));
            for entry in recent.iter().take(8) {
                let name = entry
                    .path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| entry.path.display().to_string());
                if ui.selectable_label(false, &name).on_hover_text(entry.path.display().to_string()).clicked() {
                    *selection = ComparePickerSource::Filesystem(entry.path.clone());
                }
            }
        }
        ui.separator();
        if ui.button(hxy_i18n::t("compare-picker-browse")).clicked()
            && let Some(path) = rfd::FileDialog::new().pick_file()
        {
            *selection = ComparePickerSource::Filesystem(path);
        }
    });
}

/// Resolve `picker` into bytes for both sides and push a fresh
/// `Tab::Compare`. Reads filesystem sources synchronously -- v1 only;
/// large-file streaming is a follow-up.
fn spawn_compare_from_picker(
    app: &mut HxyApp,
    ctx: &egui::Context,
    state: &ComparePickerState,
) -> Result<(), CompareSpawnError> {
    let a = read_picker_source(app, &state.a)?;
    let b = read_picker_source(app, &state.b)?;
    let id = crate::compare::CompareId::new(app.next_compare_id);
    app.next_compare_id += 1;
    let mut session = crate::compare::CompareSession::new(
        id,
        crate::compare::ComparePane::from_bytes(a.name, a.source, a.bytes),
        crate::compare::ComparePane::from_bytes(b.name, b.source, b.bytes),
    );
    let deadline = session.effective_deadline(app.state.read().app.compare_recompute_deadline);
    session.request_recompute(ctx, deadline);
    app.compares.insert(id, session);
    app.dock.push_to_focused_leaf(Tab::Compare(id));
    if let Some(path) = app.dock.find_tab(&Tab::Compare(id)) {
        crate::app::remove_welcome_from_leaf(&mut app.dock, path.surface, path.node);
    }
    Ok(())
}

fn read_picker_source(app: &HxyApp, side: &ComparePickerSource) -> Result<PickedSide, CompareSpawnError> {
    match side {
        ComparePickerSource::Unset => Err(CompareSpawnError::Unset),
        ComparePickerSource::OpenFile(id) => {
            let file = app.files.get(id).ok_or(CompareSpawnError::OpenFileGone(id.get()))?;
            let len = file.editor.source().len().get();
            let bytes = if len == 0 {
                Vec::new()
            } else {
                let range = hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(len))
                    .map_err(|e| CompareSpawnError::ReadOpenFile(e.to_string()))?;
                file.editor.source().read(range).map_err(|e| CompareSpawnError::ReadOpenFile(e.to_string()))?
            };
            Ok(PickedSide { name: file.display_name.clone(), source: file.source_kind.clone(), bytes })
        }
        ComparePickerSource::Filesystem(path) => {
            let bytes =
                std::fs::read(path).map_err(|source| CompareSpawnError::ReadFile { path: path.clone(), source })?;
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            Ok(PickedSide { name, source: Some(TabSource::Filesystem(path.clone())), bytes })
        }
    }
}

/// Palette entry point: the user picked two open-tab sources from the
/// command palette and wants a Compare tab spawned without going
/// through the modal. Reuses [`HxyApp::spawn_compare_from_sources`].
pub fn spawn_compare_from_palette(app: &mut HxyApp, ctx: &egui::Context, a: TabSource, b: TabSource) {
    let global_deadline = app.state.read().app.compare_recompute_deadline;
    match app.spawn_compare_from_sources(a, b) {
        Ok(id) => {
            if let Some(session) = app.compares.get_mut(&id) {
                let deadline = session.effective_deadline(global_deadline);
                session.request_recompute(ctx, deadline);
            }
            app.dock.push_to_focused_leaf(Tab::Compare(id));
            if let Some(path) = app.dock.find_tab(&Tab::Compare(id)) {
                crate::app::remove_welcome_from_leaf(&mut app.dock, path.surface, path.node);
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "spawn compare from palette");
        }
    }
}
