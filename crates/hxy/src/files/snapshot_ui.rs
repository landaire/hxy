//! Snapshot management dialog: list / rename / delete / compare
//! the captures stored on an open file.
//!
//! Most of the heavy lifting lives in
//! [`crate::files::snapshot::SnapshotStore`]; this module is just
//! the per-frame dialog renderer plus the compare-spawn
//! integration that turns a snapshot pair into a `Tab::Compare`.

#![cfg(not(target_arch = "wasm32"))]

use crate::app::HxyApp;
use crate::files::FileId;
use crate::files::snapshot::SnapshotId;
use crate::tabs::Tab;

/// Live state for the per-frame snapshot manager dialog. The
/// host stashes one of these on `HxyApp::pending_snapshot_dialog`
/// to keep it open across frames; `None` hides the dialog.
pub struct SnapshotDialogState {
    pub file_id: FileId,
    /// Current selection for the "compare A vs B" picker at the
    /// bottom of the dialog. `None` on either side means the
    /// user hasn't picked yet.
    pub compare_a: Option<SnapshotId>,
    pub compare_b: Option<SnapshotComparePick>,
    /// User-typed name for the next "Take snapshot" button
    /// click. Cleared after each successful capture.
    pub pending_name: String,
    /// Snapshot id whose name field is currently in edit mode,
    /// plus the in-progress new name. None means no rename is
    /// active.
    pub renaming: Option<(SnapshotId, String)>,
}

/// One side of the compare-pair picker. "Current" diffs the
/// snapshot against whatever the editor currently shows
/// (patches included).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotComparePick {
    Current,
    Snapshot(SnapshotId),
}

impl SnapshotDialogState {
    pub fn new(file_id: FileId) -> Self {
        Self { file_id, compare_a: None, compare_b: None, pending_name: String::new(), renaming: None }
    }
}

/// Open (or focus) the snapshot dialog for `file_id`. Idempotent
/// when the dialog is already targeted at the same file.
pub fn open_for(app: &mut HxyApp, file_id: FileId) {
    if matches!(&app.pending_snapshot_dialog, Some(s) if s.file_id == file_id) {
        return;
    }
    app.pending_snapshot_dialog = Some(SnapshotDialogState::new(file_id));
}

/// Take a snapshot of the file's current patched bytes with the
/// provided (or auto-generated) name. Returns the new snapshot
/// id on success; logs and returns `None` on failure.
pub fn capture_snapshot(app: &mut HxyApp, file_id: FileId, name: String) -> Option<SnapshotId> {
    let display = app.files.get(&file_id).map(|f| f.display_name.clone()).unwrap_or_default();
    let ctx_label = format!("Snapshot {display}");
    let bytes = match read_current_bytes(app, file_id) {
        Ok(b) => b,
        Err(e) => {
            app.console_log(crate::app::ConsoleSeverity::Error, ctx_label, format!("read bytes: {e}"));
            return None;
        }
    };
    let file = app.files.get_mut(&file_id)?;
    let store = file.snapshots.as_mut()?;
    match store.capture(name, bytes) {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::warn!(error = %e, file_id = file_id.get(), "capture snapshot");
            None
        }
    }
}

/// Spawn a `Tab::Compare` between two snapshot picks (current or
/// stored snapshot). Picks the global recompute deadline as the
/// initial budget. No-op when the file is gone or either side
/// can't be resolved into bytes.
pub fn spawn_compare_for_snapshots(
    app: &mut HxyApp,
    ctx: &egui::Context,
    file_id: FileId,
    a: SnapshotComparePick,
    b: SnapshotComparePick,
) {
    let Some((a_name, a_bytes)) = resolve_pick(app, file_id, a) else { return };
    let Some((b_name, b_bytes)) = resolve_pick(app, file_id, b) else { return };
    let display_name = app.files.get(&file_id).map(|f| f.display_name.clone()).unwrap_or_default();
    // Tag the source so the compare tab's title carries the
    // hosting file's identity but each pane reads from a frozen
    // anonymous buffer rather than the live editor.
    let source = app.files.get(&file_id).and_then(|f| f.source_kind.clone());
    let id = crate::compare::CompareId::new(app.next_compare_id);
    app.next_compare_id += 1;
    let mut session = crate::compare::CompareSession::new(
        id,
        crate::compare::ComparePane::from_bytes(format!("{display_name} :: {a_name}"), source.clone(), a_bytes),
        crate::compare::ComparePane::from_bytes(format!("{display_name} :: {b_name}"), source, b_bytes),
    );
    let global_deadline = app.state.read().app.compare_recompute_deadline;
    let deadline = session.effective_deadline(global_deadline);
    session.request_recompute(ctx, deadline);
    app.compares.insert(id, session);
    app.dock.push_to_focused_leaf(Tab::Compare(id));
    if let Some(path) = app.dock.find_tab(&Tab::Compare(id)) {
        crate::tabs::dock_ops::remove_welcome_from_leaf(&mut app.dock, path.surface, path.node);
    }
}

fn resolve_pick(app: &HxyApp, file_id: FileId, pick: SnapshotComparePick) -> Option<(String, Vec<u8>)> {
    match pick {
        SnapshotComparePick::Current => {
            let bytes = read_current_bytes(app, file_id).ok()?;
            Some((hxy_i18n::t("snapshot-pick-current"), bytes))
        }
        SnapshotComparePick::Snapshot(id) => {
            let file = app.files.get(&file_id)?;
            let store = file.snapshots.as_ref()?;
            let snap = store.get(id)?;
            let bytes = snap.load_bytes().ok()?;
            // Clone out of the Arc so the CompareSession owns
            // its own copy -- the editor / patch overlay
            // expects unique-bytes.
            Some((snap.name.clone(), (*bytes).clone()))
        }
    }
}

fn read_current_bytes(app: &HxyApp, file_id: FileId) -> std::io::Result<Vec<u8>> {
    let file = app.files.get(&file_id).ok_or_else(|| std::io::Error::other("file missing"))?;
    let len = file.editor.source().len().get();
    if len == 0 {
        return Ok(Vec::new());
    }
    let range = hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(len))
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    file.editor.source().read(range).map_err(|e| std::io::Error::other(e.to_string()))
}

/// Render the snapshot manager modal. Drives capture / rename /
/// delete / compare actions through the helpers above so the
/// per-frame surface stays small.
pub fn render_snapshot_dialog(ctx: &egui::Context, app: &mut HxyApp) {
    let Some(state) = app.pending_snapshot_dialog.as_ref() else { return };
    let file_id = state.file_id;
    let display_name = app.files.get(&file_id).map(|f| f.display_name.clone()).unwrap_or_default();

    // Snapshot the static parts the dialog needs to read so we
    // can surrender the borrow on `app.pending_snapshot_dialog`
    // and mutate `app.files.snapshots` from button handlers.
    let snapshot_specs: Vec<SnapshotRowSpec> = {
        let Some(file) = app.files.get(&file_id) else {
            // File was closed underneath us; close the dialog.
            app.pending_snapshot_dialog = None;
            return;
        };
        match file.snapshots.as_ref() {
            Some(store) => store
                .snapshots
                .iter()
                .map(|s| SnapshotRowSpec {
                    id: s.id,
                    name: s.name.clone(),
                    byte_len: s.byte_len,
                    captured_at: s.captured_at,
                    cached: s.is_cached(),
                })
                .collect(),
            None => Vec::new(),
        }
    };
    let store_present = app.files.get(&file_id).and_then(|f| f.snapshots.as_ref()).is_some();

    let mut keep_open = true;
    let mut take_clicked: Option<String> = None;
    let mut rename_commit: Option<(SnapshotId, String)> = None;
    let mut rename_start: Option<SnapshotId> = None;
    let mut rename_cancel = false;
    let mut delete_clicked: Option<SnapshotId> = None;
    let mut compare_with_current_clicked: Option<SnapshotId> = None;
    let mut compare_pair_clicked = false;

    egui::Window::new(hxy_i18n::t_args("snapshot-dialog-title", &[("name", &display_name)]))
        .id(egui::Id::new("hxy_snapshot_dialog"))
        .collapsible(false)
        .resizable(true)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .open(&mut keep_open)
        .show(ctx, |ui| {
            ui.set_min_width(520.0);

            if !store_present {
                ui.label(hxy_i18n::t("snapshot-no-store"));
                return;
            }

            let dialog = app.pending_snapshot_dialog.as_mut().expect("checked above");
            ui.horizontal(|ui| {
                ui.label(hxy_i18n::t("snapshot-take-label"));
                ui.add(
                    egui::TextEdit::singleline(&mut dialog.pending_name)
                        .hint_text(hxy_i18n::t("snapshot-take-name-hint"))
                        .desired_width(220.0),
                );
                if ui.button(hxy_i18n::t("snapshot-take-button")).clicked() {
                    take_clicked = Some(std::mem::take(&mut dialog.pending_name));
                }
            });
            ui.add_space(6.0);
            ui.separator();

            if snapshot_specs.is_empty() {
                ui.add_space(4.0);
                ui.label(hxy_i18n::t("snapshot-empty"));
            } else {
                let active_rename = dialog.renaming.clone();
                egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
                    egui::Grid::new("hxy-snapshot-grid").num_columns(5).striped(true).show(ui, |ui| {
                        for snap in &snapshot_specs {
                            let renaming_this = active_rename.as_ref().map(|(id, _)| *id) == Some(snap.id);
                            if renaming_this {
                                let dialog = app.pending_snapshot_dialog.as_mut().expect("checked above");
                                let buf = dialog
                                    .renaming
                                    .as_mut()
                                    .map(|(_, b)| b)
                                    .expect("renaming_this implies renaming is Some");
                                let text =
                                    ui.add(egui::TextEdit::singleline(buf).desired_width(180.0)).on_hover_text(
                                        hxy_i18n::t("snapshot-rename-hint"),
                                    );
                                if text.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                    rename_commit = Some((snap.id, buf.clone()));
                                }
                                if text.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                                    rename_cancel = true;
                                }
                            } else {
                                let label = ui.add(egui::Label::new(&snap.name).sense(egui::Sense::click()));
                                if label.double_clicked() {
                                    rename_start = Some(snap.id);
                                }
                            }
                            ui.label(format_size_summary(snap.byte_len, snap.cached));
                            ui.label(format_captured_at(snap.captured_at));
                            if ui
                                .button(hxy_i18n::t("snapshot-compare-current"))
                                .on_hover_text(hxy_i18n::t("snapshot-compare-current-tooltip"))
                                .clicked()
                            {
                                compare_with_current_clicked = Some(snap.id);
                            }
                            if ui
                                .button(hxy_i18n::t("snapshot-delete"))
                                .on_hover_text(hxy_i18n::t("snapshot-delete-tooltip"))
                                .clicked()
                            {
                                delete_clicked = Some(snap.id);
                            }
                            ui.end_row();
                        }
                    });
                });

                ui.add_space(8.0);
                ui.separator();
                ui.label(hxy_i18n::t("snapshot-pair-header"));
                ui.horizontal(|ui| {
                    let dialog = app.pending_snapshot_dialog.as_mut().expect("checked above");
                    ui.label("A:");
                    snapshot_picker(ui, "snap-a", &snapshot_specs, &mut dialog.compare_a);
                    ui.label("B:");
                    snapshot_pair_picker(ui, "snap-b", &snapshot_specs, &mut dialog.compare_b);
                    let ready = dialog.compare_a.is_some() && dialog.compare_b.is_some();
                    let response = ui.add_enabled(ready, egui::Button::new(hxy_i18n::t("snapshot-compare-pair")));
                    if response.clicked() {
                        compare_pair_clicked = true;
                    }
                });
            }
        });

    // Apply each click outside the egui borrow so HxyApp helpers
    // can mutate `app.files` / `app.compares` freely.
    if let Some(name) = take_clicked {
        capture_snapshot(app, file_id, name);
    }
    if let Some(id) = rename_start
        && let Some(state) = app.pending_snapshot_dialog.as_mut()
    {
        let initial = snapshot_specs.iter().find(|s| s.id == id).map(|s| s.name.clone()).unwrap_or_default();
        state.renaming = Some((id, initial));
    }
    if rename_cancel
        && let Some(state) = app.pending_snapshot_dialog.as_mut()
    {
        state.renaming = None;
    }
    if let Some((id, name)) = rename_commit {
        if let Some(file) = app.files.get_mut(&file_id)
            && let Some(store) = file.snapshots.as_mut()
        {
            store.rename(id, name);
        }
        if let Some(state) = app.pending_snapshot_dialog.as_mut() {
            state.renaming = None;
        }
    }
    if let Some(id) = delete_clicked
        && let Some(file) = app.files.get_mut(&file_id)
        && let Some(store) = file.snapshots.as_mut()
    {
        store.delete(id);
    }
    if let Some(id) = compare_with_current_clicked {
        spawn_compare_for_snapshots(app, ctx, file_id, SnapshotComparePick::Snapshot(id), SnapshotComparePick::Current);
    }
    if compare_pair_clicked
        && let Some(state) = app.pending_snapshot_dialog.as_ref()
        && let (Some(a), Some(b)) = (state.compare_a, state.compare_b)
    {
        spawn_compare_for_snapshots(app, ctx, file_id, SnapshotComparePick::Snapshot(a), b);
    }
    if !keep_open {
        app.pending_snapshot_dialog = None;
    }
}

/// Cheap snapshot of a row used by the dialog renderer so we can
/// drop the `app.files` borrow before applying click outcomes.
struct SnapshotRowSpec {
    id: SnapshotId,
    name: String,
    byte_len: u64,
    captured_at: jiff::Timestamp,
    cached: bool,
}

fn format_size_summary(bytes: u64, cached: bool) -> String {
    let size = format_bytes(bytes);
    if cached {
        hxy_i18n::t_args("snapshot-size-cached", &[("size", &size)])
    } else {
        hxy_i18n::t_args("snapshot-size-disk", &[("size", &size)])
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn format_captured_at(ts: jiff::Timestamp) -> String {
    // Localised display would need timezone resolution; the
    // dialog uses a compact UTC string for v1.
    ts.strftime("%Y-%m-%d %H:%M").to_string()
}

fn snapshot_picker(ui: &mut egui::Ui, salt: &str, specs: &[SnapshotRowSpec], current: &mut Option<SnapshotId>) {
    let label = match current.and_then(|id| specs.iter().find(|s| s.id == id)) {
        Some(s) => s.name.clone(),
        None => hxy_i18n::t("snapshot-pick-empty"),
    };
    egui::ComboBox::from_id_salt(salt).selected_text(label).show_ui(ui, |ui| {
        for snap in specs {
            ui.selectable_value(current, Some(snap.id), &snap.name);
        }
    });
}

fn snapshot_pair_picker(
    ui: &mut egui::Ui,
    salt: &str,
    specs: &[SnapshotRowSpec],
    current: &mut Option<SnapshotComparePick>,
) {
    let label = match current {
        Some(SnapshotComparePick::Current) => hxy_i18n::t("snapshot-pick-current"),
        Some(SnapshotComparePick::Snapshot(id)) => {
            specs.iter().find(|s| s.id == *id).map(|s| s.name.clone()).unwrap_or_else(|| hxy_i18n::t("snapshot-pick-empty"))
        }
        None => hxy_i18n::t("snapshot-pick-empty"),
    };
    egui::ComboBox::from_id_salt(salt).selected_text(label).show_ui(ui, |ui| {
        ui.selectable_value(current, Some(SnapshotComparePick::Current), hxy_i18n::t("snapshot-pick-current"));
        for snap in specs {
            ui.selectable_value(current, Some(SnapshotComparePick::Snapshot(snap.id)), &snap.name);
        }
    });
}

