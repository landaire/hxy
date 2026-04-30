//! Browser build of `HxyApp`.
//!
//! TEMPORARY: this is a slimmed-down `HxyApp` carrying only the
//! features that already compile cleanly under `wasm32-unknown-unknown`.
//! The desktop build's `HxyApp` lives in [`crate::app`]; both are
//! re-exported as `HxyApp` from the crate root so call sites
//! (`main.rs`, `wasm.rs`, anything that holds an `HxyApp`) refer
//! to one symbolic type.
//!
//! The end state collapses these two implementations into one --
//! the desktop modules need cfg-gating pushed inward (most fields
//! and methods don't fundamentally need a target gate, just the
//! ones touching plugin host / filesystem / IPC). Until that
//! refactor lands, this module exists as a stepping stone so we
//! can ship a working browser build incrementally rather than
//! blocking on the whole codebase.
//!
//! Features carried over so far: hex view of an in-memory buffer,
//! file open / save via `rfd::AsyncFileDialog` (open: pick a file,
//! save: trigger a browser download), drag-and-drop file open,
//! Cmd+F local search bar (find / find-all; replace flows are
//! desktop-only until the patch / modal scaffolding gets ungated),
//! multi-tab dock. Everything else (panels, palette, plugins,
//! templates, visualizers) follows in subsequent commits.

// `lib.rs` already gates this module to `target_arch = "wasm32"`;
// no inner `#![cfg]` -- clippy flags it as a duplicated attribute.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;

use egui_dock::DockArea;
use egui_dock::DockState;
use egui_dock::TabViewer;
use egui_dock::tab_viewer::OnCloseResponse;
use hxy_core::ByteCache;
use hxy_core::ByteOffset;
use hxy_core::CacheLimit;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_core::Selection;

use crate::files::FileId;
use crate::files::OpenFile;
use crate::search::bar::SearchEvent;
use crate::search::find_all;
use crate::search::find_next;
use crate::search::find_prev;
use crate::state::SharedPersistedState;
use crate::tabs::Tab;

/// Browser-side closed-tab snapshot. Holds the actual bytes
/// because there's no disk to re-read from on Cmd+Shift+T --
/// the desktop equivalent only saves a `TabSource` and re-opens
/// the file on restore. Selection / scroll come along so the
/// reopened tab lands where the user left it. See
/// `feedback_wasm_persistence_policy` in memory: in-memory
/// closed-tab buffer is the only state we keep across closes.
struct ClosedTab {
    name: String,
    bytes: Vec<u8>,
    selection: Option<Selection>,
    scroll_offset: f32,
}

const CLOSED_TABS_CAPACITY: usize = 32;

pub struct HxyApp {
    dock: DockState<Tab>,
    files: HashMap<FileId, OpenFile>,
    next_file_id: u64,
    state: SharedPersistedState,
    byte_cache: Arc<ByteCache>,
    last_active_file: Option<FileId>,
    applied_zoom: f32,
    /// LIFO buffer of recently-closed file tabs the user can pop
    /// back via Cmd+Shift+T. Capped at [`CLOSED_TABS_CAPACITY`].
    closed_tabs: VecDeque<ClosedTab>,
}

impl HxyApp {
    pub fn new(cc: &eframe::CreationContext<'_>, state: SharedPersistedState) -> Self {
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        cc.egui_ctx.set_global_style(crate::style::hxy_style());
        let initial_zoom = state.read().app.zoom_factor;
        cc.egui_ctx.set_zoom_factor(initial_zoom);
        let limit = CacheLimit::from_mib(state.read().app.byte_cache_limit_mib);
        Self {
            dock: DockState::new(vec![Tab::Welcome]),
            files: HashMap::new(),
            next_file_id: 1,
            state,
            byte_cache: ByteCache::new(limit),
            last_active_file: None,
            applied_zoom: initial_zoom,
            closed_tabs: VecDeque::with_capacity(CLOSED_TABS_CAPACITY),
        }
    }

    /// Close `id`'s file tab. Reads back the live bytes through
    /// the editor (so any in-memory edits are captured) and
    /// pushes the snapshot onto `closed_tabs` for Cmd+Shift+T.
    /// No-op when `id` isn't an open file.
    fn close_file_tab(&mut self, id: FileId) {
        let Some(file) = self.files.get(&id) else { return };
        let len = file.editor.source().len().get();
        let bytes = if len == 0 {
            Vec::new()
        } else if let Ok(range) = hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(len))
        {
            file.editor.source().read(range).unwrap_or_default()
        } else {
            Vec::new()
        };
        let snap = ClosedTab {
            name: file.display_name.clone(),
            bytes,
            selection: file.editor.selection(),
            scroll_offset: file.editor.scroll_offset(),
        };
        if self.closed_tabs.len() >= CLOSED_TABS_CAPACITY {
            self.closed_tabs.pop_front();
        }
        self.closed_tabs.push_back(snap);
        if let Some(path) = self.dock.find_tab(&Tab::File(id)) {
            let _ = self.dock.remove_tab(path);
        }
        if let Some(removed) = self.files.remove(&id) {
            removed.release_cache();
        }
        if self.last_active_file == Some(id) {
            self.last_active_file = None;
        }
    }

    /// Open or close `tab` in the dock. If the tab is already
    /// present, focus it; otherwise push it as a new dock leaf.
    /// Used by the toolbar Strings / Checksums / Entropy /
    /// Inspector buttons. The desktop build routes these through
    /// `dock_ops::push_tool_tab`, which keeps tool panels in a
    /// dedicated right-hand leaf -- the wasm build doesn't have
    /// that infrastructure ungated yet, so for now everything
    /// pushes to the focused leaf.
    fn toggle_tab(&mut self, tab: Tab) {
        if let Some(path) = self.dock.find_tab(&tab) {
            let _ = self.dock.set_active_tab(path);
            return;
        }
        self.dock.push_to_focused_leaf(tab);
    }

    /// Pop the most recently closed tab off the LIFO buffer and
    /// re-open it as a fresh `Tab::File`, restoring its
    /// selection + scroll. No-op when the buffer is empty.
    fn reopen_last_closed(&mut self) {
        let Some(snap) = self.closed_tabs.pop_back() else { return };
        let id = self.open_bytes(snap.name, snap.bytes);
        if let Some(file) = self.files.get_mut(&id) {
            file.editor.set_selection(snap.selection);
            file.editor.set_scroll_to(snap.scroll_offset);
        }
    }

    /// Format the active file's current selection as a clipboard
    /// string. `as_hex = false` returns a UTF-8 lossy decode of
    /// the bytes (matching desktop's `BytesLossyUtf8` copy kind);
    /// `as_hex = true` formats as space-separated upper-case hex
    /// pairs (`BytesHexSpaced`). Returns `None` when no file is
    /// focused or the selection is empty / a caret. The richer
    /// "Copy as struct / Copy value as ..." formats land later
    /// alongside the desktop's [`crate::files::copy`] module
    /// (currently desktop-gated).
    fn copy_active_selection(&self, as_hex: bool) -> Option<String> {
        let id = self.last_active_file?;
        let file = self.files.get(&id)?;
        let sel = file.editor.selection()?;
        let range = sel.range();
        if range.is_empty() {
            return None;
        }
        let bytes = file.editor.source().read(range).ok()?;
        if as_hex {
            let mut out = String::with_capacity(bytes.len() * 3);
            for (i, b) in bytes.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                out.push_str(&format!("{b:02X}"));
            }
            Some(out)
        } else {
            Some(String::from_utf8_lossy(&bytes).into_owned())
        }
    }

    /// Snapshot the active file's display name and byte source for
    /// the wasm save flow. Reads the entire source through the
    /// editor (so any in-memory patches are included). Returns
    /// `None` when no file is focused or the source can't be read.
    /// Bytes are buffered up front because the `rfd::save_file`
    /// future runs on a separate `spawn_local` task and can't
    /// re-borrow `&self.files`.
    fn active_file_bytes(&self) -> Option<(String, Vec<u8>)> {
        let id = self.last_active_file?;
        let file = self.files.get(&id)?;
        let len = file.editor.source().len().get();
        let bytes = if len == 0 {
            Vec::new()
        } else {
            let range = hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(len)).ok()?;
            file.editor.source().read(range).ok()?
        };
        Some((file.display_name.clone(), bytes))
    }

    /// Open an in-memory byte buffer as a fresh file tab. Used by
    /// the rfd file picker and any future drag-and-drop / paste
    /// path. Returns the new tab's id.
    pub fn open_bytes(&mut self, name: String, bytes: Vec<u8>) -> FileId {
        let id = FileId::new(self.next_file_id);
        self.next_file_id += 1;
        let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
        let file = OpenFile::from_source(id, name, None, source, &self.byte_cache);
        self.files.insert(id, file);
        self.dock.push_to_focused_leaf(Tab::File(id));
        if let Some(path) = self.dock.find_tab(&Tab::Welcome) {
            let _ = self.dock.remove_tab(path);
        }
        self.last_active_file = Some(id);
        id
    }
}

impl eframe::App for HxyApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Apply any zoom change the settings panel pushed (no
        // settings panel yet on wasm, but the field is wired
        // through PersistedState so a future palette command
        // can drive it).
        let target_zoom = self.state.read().app.zoom_factor;
        if (target_zoom - self.applied_zoom).abs() > f32::EPSILON {
            ctx.set_zoom_factor(target_zoom);
            self.applied_zoom = target_zoom;
        }
        // Drag-and-drop file open. egui's `dropped_files` carries
        // the file name + bytes for each file the user dropped on
        // the canvas this frame; on wasm `bytes` is always
        // populated (browser file API), `path` is `None`.
        let dropped: Vec<egui::DroppedFile> = ctx.input(|i| i.raw.dropped_files.clone());
        for f in dropped {
            let bytes = match f.bytes {
                Some(b) => b.to_vec(),
                None => continue,
            };
            let name = if f.name.is_empty() { "dropped".to_owned() } else { f.name };
            self.open_bytes(name, bytes);
        }
        // Keyboard shortcuts. Cmd/Ctrl maps to egui's
        // `Modifiers::COMMAND` regardless of platform.
        let (toggle_find, close_tab, reopen_tab, copy_bytes, copy_hex) = ctx.input_mut(|i| {
            (
                i.consume_shortcut(&egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::F)),
                i.consume_shortcut(&egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::W)),
                i.consume_shortcut(&egui::KeyboardShortcut::new(
                    egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
                    egui::Key::T,
                )),
                i.consume_shortcut(&egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::C)),
                i.consume_shortcut(&egui::KeyboardShortcut::new(
                    egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
                    egui::Key::C,
                )),
            )
        });
        if toggle_find
            && let Some(id) = self.last_active_file
            && let Some(file) = self.files.get_mut(&id)
        {
            file.search.open = !file.search.open;
            if file.search.open {
                file.search.refresh_pattern();
            }
        }
        if close_tab && let Some(id) = self.last_active_file {
            self.close_file_tab(id);
        }
        if reopen_tab {
            self.reopen_last_closed();
        }
        if (copy_bytes || copy_hex)
            && let Some(text) = self.copy_active_selection(copy_hex)
        {
            ctx.copy_text(text);
        }
        // Route un-consumed keyboard events into the active file's
        // hex view: arrow navigation, page up/down, hex-nibble
        // typing in edit mode, etc. The editor only acts when no
        // other widget holds keyboard focus.
        if let Some(id) = self.last_active_file
            && let Some(file) = self.files.get_mut(&id)
        {
            file.editor.handle_input(&ctx);
        }
        egui::Panel::top("hxy_top_bar").show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("New").clicked() {
                    self.open_bytes("Untitled".to_owned(), Vec::new());
                }
                if ui.button("Open files...").clicked() {
                    let ctx_clone = ctx.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        // Multi-pick: each picked file lands as
                        // its own tab. The browser file picker
                        // returns an empty list on cancel.
                        let Some(handles) = rfd::AsyncFileDialog::new().pick_files().await else {
                            return;
                        };
                        for handle in handles {
                            let bytes = handle.read().await;
                            let name = handle.file_name();
                            push_open_request(name, bytes);
                        }
                        ctx_clone.request_repaint();
                    });
                }
                // Save the active file's current bytes (post any
                // in-memory edits) as a browser download. rfd's
                // wasm save backend doesn't pop a save-as dialog
                // -- it returns a writable handle whose `.write`
                // call triggers the download with the suggested
                // filename baked in.
                let snapshot = self.active_file_bytes();
                ui.add_enabled_ui(snapshot.is_some(), |ui| {
                    if ui.button("Save as...").clicked()
                        && let Some((name, bytes)) = snapshot
                    {
                        wasm_bindgen_futures::spawn_local(async move {
                            let Some(handle) = rfd::AsyncFileDialog::new().set_file_name(&name).save_file().await
                            else {
                                return;
                            };
                            if let Err(e) = handle.write(&bytes).await {
                                tracing::warn!(error = %e, "wasm save");
                            }
                        });
                    }
                });
                // Per-file analysis tabs. Each toggles the
                // corresponding panel for the active file (or
                // focuses an existing tab). Disabled when no file
                // is open.
                let active_id = self.last_active_file;
                ui.add_enabled_ui(active_id.is_some(), |ui| {
                    if ui.button("Strings").clicked()
                        && let Some(id) = active_id
                    {
                        self.toggle_tab(Tab::Strings(id));
                    }
                    if ui.button("Checksums").clicked()
                        && let Some(id) = active_id
                    {
                        self.toggle_tab(Tab::Checksums(id));
                    }
                    if ui.button("Entropy").clicked()
                        && let Some(id) = active_id
                    {
                        self.toggle_tab(Tab::Entropy(id));
                    }
                });
                if ui.button("Inspector").clicked() {
                    self.toggle_tab(Tab::Inspector);
                }
                ui.label(crate::APP_NAME);
            });
        });
        // Drain any rfd-driven open requests posted from the
        // async picker callback. Done here (UI thread) so the
        // file insertion sees a `&mut self`.
        for (name, bytes) in drain_open_requests() {
            self.open_bytes(name, bytes);
        }
        let mut pending_close: Vec<FileId> = Vec::new();
        egui::CentralPanel::default().show_inside(ui, |ui| {
            let style = crate::style::hxy_dock_style(ui.style());
            DockArea::new(&mut self.dock).style(style).show_inside(
                ui,
                &mut WasmTabViewer {
                    files: &mut self.files,
                    last_active_file: &mut self.last_active_file,
                    byte_cache: &self.byte_cache,
                    pending_close: &mut pending_close,
                },
            );
        });
        for id in pending_close {
            self.close_file_tab(id);
        }
    }
}

/// Per-frame TabViewer for the wasm build. The desktop build's
/// `HxyTabViewer` carries enough refs to render every panel kind;
/// here we just paint the hex view (or a Welcome placeholder) and
/// leave the other Tab variants as no-ops for now.
struct WasmTabViewer<'a> {
    files: &'a mut HashMap<FileId, OpenFile>,
    last_active_file: &'a mut Option<FileId>,
    byte_cache: &'a Arc<ByteCache>,
    /// Drained after the dock pass: each entry is a File tab the
    /// user X-clicked. The host then runs `close_file_tab`, which
    /// captures the byte snapshot for the reopen buffer and frees
    /// the `OpenFile`.
    pending_close: &'a mut Vec<FileId>,
}

/// Read a small window of bytes around the active tab's caret
/// for the data inspector. Returns `(caret_offset, bytes)` so
/// the inspector can lay them out as integers / floats / etc.
/// Returns `None` when the file has no selection or the read
/// fails.
fn inspector_window(file: &OpenFile) -> Option<(ByteOffset, Vec<u8>)> {
    let sel = file.editor.selection()?;
    let caret = sel.cursor;
    let total = file.editor.source().len().get();
    if total == 0 {
        return None;
    }
    let start = caret.get();
    let end = (start + 16).min(total);
    let range = hxy_core::ByteRange::new(caret, ByteOffset::new(end)).ok()?;
    let bytes = file.editor.source().read(range).ok()?;
    Some((caret, bytes))
}

impl TabViewer for WasmTabViewer<'_> {
    type Tab = Tab;

    fn closeable(&mut self, tab: &mut Self::Tab) -> bool {
        matches!(tab, Tab::File(_))
    }

    fn on_close(&mut self, tab: &mut Self::Tab) -> OnCloseResponse {
        if let Tab::File(id) = tab {
            self.pending_close.push(*id);
            // We close the tab ourselves after the dock pass so
            // the snapshot capture sees the still-live OpenFile.
            // Returning `Ignore` keeps the dock from removing the
            // tab right now; the host's drain does it.
            OnCloseResponse::Ignore
        } else {
            OnCloseResponse::Close
        }
    }

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        let panel_title = |id: &FileId, label: &str| -> egui::WidgetText {
            let name = self.files.get(id).map(|f| f.display_name.as_str()).unwrap_or("(missing)");
            format!("{label} ({name})").into()
        };
        match tab {
            Tab::Welcome => "Welcome".into(),
            Tab::Settings => "Settings".into(),
            Tab::Console => "Console".into(),
            Tab::Inspector => "Inspector".into(),
            Tab::Plugins => "Plugins".into(),
            Tab::Memory => "Memory".into(),
            Tab::File(id) => match self.files.get(id) {
                Some(f) => f.display_name.clone().into(),
                None => format!("file-{}", id.get()).into(),
            },
            Tab::Workspace(id) => format!("workspace-{}", id.get()).into(),
            Tab::Entropy(id) => panel_title(id, "Entropy"),
            Tab::Strings(id) => panel_title(id, "Strings"),
            Tab::Checksums(id) => panel_title(id, "Checksums"),
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        match tab {
            Tab::Welcome => {
                ui.heading("hxy");
                ui.label("Open a file from the toolbar to get started.");
            }
            Tab::File(id) => {
                let id = *id;
                if let Some(file) = self.files.get_mut(&id) {
                    *self.last_active_file = Some(id);
                    // Bottom-anchored search bar -- toggled by
                    // Cmd+F at the app level; the bar itself is
                    // a render-only widget that emits events the
                    // host applies against the file's bytes.
                    if file.search.open {
                        egui::Panel::bottom(egui::Id::new(("hxy-search-panel", id.get())))
                            .resizable(false)
                            .show_inside(ui, |ui| {
                                let events = crate::search::bar::show(ui, &mut file.search);
                                apply_search_events_readonly(file, events);
                            });
                    }
                    let len = file.editor.source().len().get();
                    if len > 0 {
                        let columns = hxy_core::ColumnCount::DEFAULT;
                        let response = file.editor.view().columns(columns).show(ui);
                        file.editor.on_response(&response, columns);
                    } else {
                        ui.label("(empty buffer)");
                    }
                } else {
                    ui.colored_label(egui::Color32::RED, format!("missing file {id:?}"));
                }
            }
            Tab::Inspector => {
                let bytes_for_inspector =
                    self.last_active_file.and_then(|id| self.files.get(&id)).and_then(|f| inspector_window(f));
                let (caret, bytes) = match bytes_for_inspector.as_ref() {
                    Some((c, b)) => (Some(c.get()), b.as_slice()),
                    None => (None, &[] as &[u8]),
                };
                // Default decoders + a transient inspector state
                // we drop after rendering; the desktop build holds
                // these on `HxyApp`. Eager allocation each frame is
                // cheap (the inspector state is small).
                let mut state = crate::panels::inspector::InspectorState::default();
                let decoders = crate::panels::inspector::default_decoders();
                crate::panels::inspector::show(ui, &mut state, &decoders, caret, bytes);
            }
            Tab::Strings(file_id) => {
                let pinned = *file_id;
                if let Some(file) = self.files.get_mut(&pinned) {
                    let label = file.display_name.clone();
                    let _ = crate::panels::strings::show(ui, Some(&label), &mut file.strings_panel);
                } else {
                    ui.colored_label(egui::Color32::RED, format!("missing file {pinned:?}"));
                }
            }
            Tab::Checksums(file_id) => {
                let pinned = *file_id;
                if let Some(file) = self.files.get_mut(&pinned) {
                    let label = file.display_name.clone();
                    let _ = crate::panels::checksums::show(ui, Some(&label), &mut file.checksums_panel);
                } else {
                    ui.colored_label(egui::Color32::RED, format!("missing file {pinned:?}"));
                }
            }
            Tab::Entropy(file_id) => {
                let pinned = *file_id;
                let (label, state, running) = match self.files.get(&pinned) {
                    Some(f) => (Some(f.display_name.as_str()), f.entropy.as_ref(), f.entropy_running.is_some()),
                    None => (None, None, false),
                };
                let mut clicked = false;
                crate::panels::entropy::show(ui, label, state, running, &mut clicked);
                // Recompute clicks are wired in a follow-up commit
                // alongside the toolbar entry-points; for now this
                // panel is a render-only view of any pre-computed
                // entropy data on the file.
            }
            Tab::Memory => {
                let labels = crate::panels::memory::ViewLabels::from_files(self.files);
                crate::panels::memory::memory_ui(ui, self.byte_cache, &labels);
            }
            other => {
                ui.label(format!("{other:?} (not yet wired on wasm)"));
            }
        }
    }
}

// rfd's `pick_file` future runs in a separate spawned task and
// can't capture `&mut HxyApp`. We use a thread-local mailbox to
// post the picked bytes back; the per-frame `update` drains it
// so insertion happens on the same thread that owns the app.
// thread-local is fine here because wasm is single-threaded
// without explicit worker setup.
type OpenRequest = (String, Vec<u8>);

thread_local! {
    static OPEN_INBOX: std::cell::RefCell<Vec<OpenRequest>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn push_open_request(name: String, bytes: Vec<u8>) {
    OPEN_INBOX.with(|q| q.borrow_mut().push((name, bytes)));
}

fn drain_open_requests() -> Vec<OpenRequest> {
    OPEN_INBOX.with(|q| std::mem::take(&mut *q.borrow_mut()))
}

/// Slim wasm-side `SearchEvent` handler. Covers only the read-only
/// events (find / scroll / scope changes); `ReplaceCurrent` /
/// `ReplaceAll` need the patch system + length-mismatch modal
/// prompts that the desktop build owns and are dropped on wasm
/// for now. The desktop equivalent is `apply_search_events` in
/// `app/mod.rs`; the eventual unified `HxyApp` will share one
/// handler with replace gated to non-wasm.
fn apply_search_events_readonly(file: &mut OpenFile, events: Vec<SearchEvent>) {
    let mut want_all = file.search.all_results;
    for ev in events {
        let bounds = file.search.scope.bounds(file.editor.source().len().get());
        match ev {
            SearchEvent::Refresh => {
                file.search.refresh_pattern();
                if want_all && let Some(p) = file.search.pattern.clone() {
                    let m = find_all(file.editor.source().as_ref(), &p, bounds);
                    let caret = current_caret(file);
                    file.search.matches = m;
                    file.search.active_idx = nearest_match_idx(&file.search.matches, caret);
                }
            }
            SearchEvent::RefreshReplace => {
                file.search.refresh_replace_pattern();
            }
            SearchEvent::Next => {
                let Some(pattern) = file.search.pattern.clone() else { continue };
                let from = current_caret(file).saturating_add(1);
                if let Some(hit) = find_next(file.editor.source().as_ref(), &pattern, from, true, bounds) {
                    apply_match_jump(file, hit.offset, &pattern);
                }
            }
            SearchEvent::Prev => {
                let Some(pattern) = file.search.pattern.clone() else { continue };
                let from = current_caret(file);
                if let Some(hit) = find_prev(file.editor.source().as_ref(), &pattern, from, true, bounds) {
                    apply_match_jump(file, hit.offset, &pattern);
                }
            }
            SearchEvent::FindAll => {
                want_all = true;
                file.search.all_results = true;
                if let Some(p) = file.search.pattern.clone() {
                    let m = find_all(file.editor.source().as_ref(), &p, bounds);
                    let caret = current_caret(file);
                    file.search.matches = m;
                    file.search.active_idx = nearest_match_idx(&file.search.matches, caret);
                    if let Some(idx) = file.search.active_idx {
                        let off = file.search.matches[idx];
                        apply_match_jump(file, off, &p);
                    }
                }
            }
            SearchEvent::ClearAll => {
                want_all = false;
                file.search.all_results = false;
                file.search.matches.clear();
                file.search.active_idx = None;
            }
            SearchEvent::Close => file.search.open = false,
            SearchEvent::JumpTo(idx) => {
                let Some(pattern) = file.search.pattern.clone() else { continue };
                let Some(off) = file.search.matches.get(idx).copied() else { continue };
                file.search.active_idx = Some(idx);
                apply_match_jump(file, off, &pattern);
            }
            SearchEvent::ToggleReplace => {
                file.search.replace_open = !file.search.replace_open;
            }
            SearchEvent::SetScope(scope) => {
                file.search.scope = scope;
                file.search.matches.clear();
                file.search.active_idx = None;
                if want_all && let Some(p) = file.search.pattern.clone() {
                    let bounds = file.search.scope.bounds(file.editor.source().len().get());
                    let m = find_all(file.editor.source().as_ref(), &p, bounds);
                    let caret = current_caret(file);
                    file.search.matches = m;
                    file.search.active_idx = nearest_match_idx(&file.search.matches, caret);
                }
            }
            SearchEvent::ReplaceCurrent | SearchEvent::ReplaceAll => {
                // Replace flows on wasm need the patch overlay
                // plus the length-mismatch / replace-all-confirm
                // modals the desktop build owns. Dropped here
                // until the unified HxyApp lands.
            }
        }
    }
}

fn current_caret(file: &OpenFile) -> u64 {
    file.editor.selection().map(|s| s.cursor.get()).unwrap_or(0)
}

fn apply_match_jump(file: &mut OpenFile, offset: u64, pattern: &[u8]) {
    let total = file.editor.source().len().get();
    if total == 0 || pattern.is_empty() {
        return;
    }
    let last = (offset + pattern.len() as u64).saturating_sub(1).min(total.saturating_sub(1));
    let anchor = ByteOffset::new(offset.min(total.saturating_sub(1)));
    let cursor = ByteOffset::new(last);
    file.editor.set_selection(Some(Selection { anchor, cursor }));
    if !file.editor.is_offset_visible(anchor) {
        file.editor.set_scroll_to_byte(anchor);
    }
}

fn nearest_match_idx(matches: &[u64], caret: u64) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    Some(matches.iter().enumerate().min_by_key(|&(_, m)| m.abs_diff(caret)).map(|(i, _)| i).unwrap_or(0))
}
