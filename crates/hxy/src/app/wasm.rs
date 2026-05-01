//! Wasm-target HxyApp impl: browser entrypoint, drag-and-drop, async
//! file open / save through `rfd::AsyncFileDialog`, the wasm-side tab
//! viewers, and panel-run drains over `egui_inbox`. Everything that
//! reaches into desktop-only state (plugin host, native menu, file
//! watcher, sync rfd, sqlite) lives in `app::desktop` instead so this
//! module compiles cleanly with `--target wasm32-unknown-unknown`.

use std::sync::Arc;

use hxy_vfs::MountedVfs;
use hxy_vfs::TabSource;
use hxy_vfs::VfsRegistry;
use hxy_vfs::handlers::ZipHandler;

use super::ConsoleEntry;
use super::HashMap;
use super::HxyApp;
use super::TabFocus;
use super::apply_global_search_events;
use super::console_ui;
use super::format_file_tab_title;
use super::format_workspace_tab_title;
use super::install_fonts;
use super::render_file_tab;
use super::settings_ui;
use super::vfs_expanded_for;
use super::welcome_ui;
use crate::files::FileId;
use crate::files::OpenFile;
use crate::state::PersistedState;
use crate::state::SharedPersistedState;
use crate::tabs::Tab;

use eframe::egui;
use egui_dock::DockState;

pub(crate) struct ClosedTabWasm {
    pub(crate) name: String,
    pub(crate) bytes: Vec<u8>,
    pub(crate) selection: Option<hxy_core::Selection>,
    pub(crate) scroll_offset: f32,
}

const CLOSED_TABS_CAPACITY_WASM: usize = 32;

impl HxyApp {
    pub fn new(cc: &eframe::CreationContext<'_>, state: SharedPersistedState) -> Self {
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        install_fonts(&cc.egui_ctx);
        cc.egui_ctx.set_global_style(crate::style::hxy_style());
        let initial_zoom = state.read().app.zoom_factor;
        cc.egui_ctx.set_zoom_factor(initial_zoom);
        let limit = hxy_core::CacheLimit::from_mib(state.read().app.byte_cache_limit_mib);
        // Same handler set the desktop registers -- the built-in
        // ZipHandler is pure Rust and works on wasm. Plugin-host-
        // backed handlers stay desktop-only.
        let mut registry = VfsRegistry::new();
        registry.register(Arc::new(ZipHandler::new()));
        Self {
            dock: DockState::new(vec![Tab::Welcome]),
            files: HashMap::new(),
            workspaces: std::collections::BTreeMap::new(),
            next_workspace_id: 1,
            state,
            next_file_id: 1,
            byte_cache: hxy_core::ByteCache::new(limit),
            registry,
            applied_zoom: initial_zoom,
            last_active_file: None,
            last_active_workspace: None,
            last_content_leaf: None,
            palette: crate::commands::palette::PaletteState::default(),
            pending_pane_pick: None,
            pane_pick_letters: std::collections::BTreeMap::new(),
            pane_pick_target_paths: None,
            tab_focus: TabFocus::Outer,
            pending_collapse_workspace: Vec::new(),
            closed_tabs: std::collections::VecDeque::with_capacity(CLOSED_TABS_CAPACITY_WASM),
            toasts: crate::toasts::ToastCenter::new(),
            pending_search_modal: None,
            global_search: crate::search::global::GlobalSearchState::default(),
            pending_global_search_events: Vec::new(),
            compares: std::collections::BTreeMap::new(),
            next_compare_id: 1,
        }
    }

    /// Open an in-memory byte buffer as a fresh file tab. Mirrors
    /// the desktop `HxyApp::open_in_memory` -> `open(_, _, _, _, _,
    /// as_workspace=false)` path: detect the VFS handler so the
    /// "Browse VFS" palette entry can light up, but the tab lands
    /// as a plain `Tab::File`. The user invokes `BrowseVfs` to
    /// mount as a workspace, same as on desktop. Auto-mounting
    /// would diverge from desktop behaviour for no good reason.
    pub fn open_bytes_wasm(&mut self, name: String, bytes: Vec<u8>) -> FileId {
        let id = FileId::new(self.next_file_id);
        self.next_file_id += 1;
        let source: Arc<dyn hxy_core::HexSource> = Arc::new(hxy_core::MemorySource::new(bytes));
        let mut file = OpenFile::from_source(id, name, None, source, &self.byte_cache);
        if let Ok(range) = hxy_core::ByteRange::new(
            hxy_core::ByteOffset::new(0),
            hxy_core::ByteOffset::new(file.editor.source().len().get().min(4096)),
        ) && let Ok(head) = file.editor.source().read(range)
        {
            file.detected_handler = self.registry.detect(&head);
        }
        self.files.insert(id, file);
        self.dock.push_to_focused_leaf(Tab::File(id));
        if let Some(path) = self.dock.find_tab(&Tab::Welcome) {
            let _ = self.dock.remove_tab(path);
        }
        self.last_active_file = Some(id);
        id
    }

    fn close_file_tab_wasm(&mut self, id: FileId) {
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
        let snap = ClosedTabWasm {
            name: file.display_name.clone(),
            bytes,
            selection: file.editor.selection(),
            scroll_offset: file.editor.scroll_offset(),
        };
        if self.closed_tabs.len() >= CLOSED_TABS_CAPACITY_WASM {
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

    fn reopen_last_closed_wasm(&mut self) {
        let Some(snap) = self.closed_tabs.pop_back() else { return };
        let id = self.open_bytes_wasm(snap.name, snap.bytes);
        if let Some(file) = self.files.get_mut(&id) {
            file.editor.set_selection(snap.selection);
            file.editor.set_scroll_to(snap.scroll_offset);
        }
    }

    fn copy_active_selection_wasm(&self, as_hex: bool) -> Option<String> {
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

    fn active_file_bytes_wasm(&self) -> Option<(String, Vec<u8>)> {
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

    fn toggle_tab_wasm(&mut self, tab: Tab) {
        if let Some(path) = self.dock.find_tab(&tab) {
            let node_path = path.node_path();
            let _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(node_path);
            return;
        }
        // Tool tabs (Inspector / Console / Memory / Strings /
        // Checksums / Entropy / Settings) land in a dedicated tool
        // pane on the right edge so the editor area stays free --
        // same `push_tool_tab` helper desktop uses. Plain content
        // tabs go to the focused leaf.
        if crate::tabs::dock_ops::is_tool_tab(&tab) {
            let node_path = crate::tabs::dock_ops::push_tool_tab(&mut self.dock, tab);
            self.dock.set_focused_node_and_surface(node_path);
        } else {
            self.dock.push_to_focused_leaf(tab);
        }
    }

    fn spawn_strings_run_wasm(&mut self, ctx: &egui::Context, id: FileId) {
        let Some(file) = self.files.get_mut(&id) else { return };
        if file.strings_panel.config.range.is_empty() {
            let len = file.editor.source().len().get();
            if let Ok(range) = hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(len)) {
                file.strings_panel.config.range = range;
            }
        }
        let source = file.editor.source().clone();
        let config = file.strings_panel.config.clone();
        file.strings_panel.running = Some(crate::panels::strings::spawn_compute(ctx, id, source, config));
    }

    fn spawn_checksums_run_wasm(&mut self, ctx: &egui::Context, id: FileId) {
        let Some(file) = self.files.get_mut(&id) else { return };
        if file.checksums_panel.config.range.is_empty() {
            let len = file.editor.source().len().get();
            if let Ok(range) = hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(len)) {
                file.checksums_panel.config.range = range;
            }
        }
        let source = file.editor.source().clone();
        let config = file.checksums_panel.config.clone();
        file.checksums_panel.running = Some(crate::panels::checksums::spawn_compute(ctx, id, source, config));
    }

    fn spawn_entropy_run_wasm(&mut self, ctx: &egui::Context, id: FileId) {
        let Some(file) = self.files.get_mut(&id) else { return };
        let source = file.editor.source().clone();
        let len = source.len().get();
        let window = crate::panels::entropy::pick_window_size(len);
        file.entropy = None;
        file.entropy_running = Some(crate::panels::entropy::spawn_compute(ctx, id, source, window));
    }

    /// Read the raw bytes of any tab the user might pick for a
    /// compare. Recognises the `/__memory__/{file_id}` synthetic
    /// path the wasm palette emits for in-memory tabs and reads
    /// from `app.files`; for any other `TabSource` returns `None`
    /// (the wasm side has no filesystem / VFS resolver).
    fn read_compare_side_wasm(&self, source: &TabSource) -> Option<(String, Vec<u8>)> {
        if let TabSource::Filesystem(path) = source {
            let s = path.to_string_lossy();
            if let Some(id_str) = s.strip_prefix("/__memory__/")
                && let Ok(id_u64) = id_str.parse::<u64>()
            {
                let id = FileId::new(id_u64);
                let file = self.files.get(&id)?;
                let len = file.editor.source().len().get();
                let bytes = if len == 0 {
                    Vec::new()
                } else {
                    let range =
                        hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(len)).ok()?;
                    file.editor.source().read(range).ok()?
                };
                return Some((file.display_name.clone(), bytes));
            }
        }
        None
    }

    /// Open a VFS entry from inside the workspace's tree as a new
    /// editor tab in the workspace's inner dock. Reads the entry
    /// stream synchronously into a `MemorySource` (the built-in
    /// ZipHandler decompresses on read, which is fine to block on
    /// in the browser's main thread for the small archives the web
    /// app deals with).
    fn open_vfs_entry_wasm(&mut self, workspace_id: crate::files::WorkspaceId, entry_path: String) {
        use std::io::Read;
        let Some(workspace) = self.workspaces.get(&workspace_id) else { return };
        let mount = workspace.mount.clone();
        let mut stream = match mount.fs.open_file(&entry_path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, entry = %entry_path, "open vfs entry");
                return;
            }
        };
        let mut bytes: Vec<u8> = Vec::new();
        if let Err(e) = stream.read_to_end(&mut bytes) {
            tracing::warn!(error = %e, entry = %entry_path, "read vfs entry");
            return;
        }
        let id = FileId::new(self.next_file_id);
        self.next_file_id += 1;
        let name = entry_path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(&entry_path).to_owned();
        let source: Arc<dyn hxy_core::HexSource> = Arc::new(hxy_core::MemorySource::new(bytes));
        let file = OpenFile::from_source(id, name, None, source, &self.byte_cache);
        self.files.insert(id, file);
        if let Some(workspace) = self.workspaces.get_mut(&workspace_id) {
            workspace.dock.push_to_focused_leaf(crate::files::WorkspaceTab::Entry(id));
        }
        self.last_active_file = Some(id);
    }

    fn jump_to_offset_wasm(&mut self, id: FileId, offset: u64, end: u64) {
        let Some(file) = self.files.get_mut(&id) else { return };
        let total = file.editor.source().len().get();
        if total == 0 {
            return;
        }
        let last = end.saturating_sub(1).min(total.saturating_sub(1));
        let anchor = hxy_core::ByteOffset::new(offset.min(total.saturating_sub(1)));
        let cursor = hxy_core::ByteOffset::new(last);
        file.editor.set_selection(Some(hxy_core::Selection { anchor, cursor }));
        if !file.editor.is_offset_visible(anchor) {
            file.editor.set_scroll_to_byte(anchor);
        }
    }

    fn drain_panel_runs_wasm(&mut self, ctx: &egui::Context) {
        let mut strings_done: Vec<(FileId, crate::panels::strings::StringsOutcome)> = Vec::new();
        let mut checksums_done: Vec<(FileId, crate::panels::checksums::ChecksumOutcome)> = Vec::new();
        let mut entropy_done: Vec<(FileId, crate::panels::entropy::EntropyOutcome)> = Vec::new();
        for (id, file) in self.files.iter_mut() {
            if let Some(run) = file.strings_panel.running.as_ref() {
                let outcomes: Vec<_> = run.inbox.read(ctx).collect();
                if !outcomes.is_empty() {
                    file.strings_panel.running = None;
                    for o in outcomes {
                        strings_done.push((*id, o));
                    }
                }
            }
            if let Some(run) = file.checksums_panel.running.as_ref() {
                let outcomes: Vec<_> = run.inbox.read(ctx).collect();
                if !outcomes.is_empty() {
                    file.checksums_panel.running = None;
                    for o in outcomes {
                        checksums_done.push((*id, o));
                    }
                }
            }
            if let Some(run) = file.entropy_running.as_ref() {
                let outcomes: Vec<_> = run.inbox.read(ctx).collect();
                if !outcomes.is_empty() {
                    file.entropy_running = None;
                    for o in outcomes {
                        entropy_done.push((*id, o));
                    }
                }
            }
        }
        for (id, outcome) in strings_done {
            let Some(file) = self.files.get_mut(&id) else { continue };
            match outcome {
                crate::panels::strings::StringsOutcome::Ok(result) => file.strings_panel.last_result = Some(result),
                crate::panels::strings::StringsOutcome::Err(msg) => tracing::warn!(error = %msg, "strings"),
            }
        }
        for (id, outcome) in checksums_done {
            let Some(file) = self.files.get_mut(&id) else { continue };
            match outcome {
                crate::panels::checksums::ChecksumOutcome::Ok(result) => {
                    file.checksums_panel.last_result = Some(result)
                }
                crate::panels::checksums::ChecksumOutcome::Err(msg) => tracing::warn!(error = %msg, "checksums"),
            }
        }
        for (id, outcome) in entropy_done {
            let Some(file) = self.files.get_mut(&id) else { continue };
            match outcome {
                crate::panels::entropy::EntropyOutcome::Ok(state) => file.entropy = Some(state),
                crate::panels::entropy::EntropyOutcome::Err(msg) => tracing::warn!(error = %msg, "entropy"),
            }
        }
    }
}

impl eframe::App for HxyApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let target_zoom = self.state.read().app.zoom_factor;
        if (target_zoom - self.applied_zoom).abs() > f32::EPSILON {
            ctx.set_zoom_factor(target_zoom);
            self.applied_zoom = target_zoom;
        }
        // Drag-and-drop file open.
        let dropped: Vec<egui::DroppedFile> = ctx.input(|i| i.raw.dropped_files.clone());
        for f in dropped {
            let bytes = match f.bytes {
                Some(b) => b.to_vec(),
                None => continue,
            };
            let name = if f.name.is_empty() { "dropped".to_owned() } else { f.name };
            self.open_bytes_wasm(name, bytes);
        }
        // Wasm-only shortcuts that don't have a shared dispatcher
        // yet: tab close (Cmd+W), reopen-closed (Cmd+Shift+T),
        // selection copy (Cmd+C / Cmd+Shift+C), palette open
        // (Cmd+Shift+P) / quick-open (Cmd+P).
        use crate::commands::shortcuts::CLOSE_TAB;
        use crate::commands::shortcuts::COMMAND_PALETTE;
        use crate::commands::shortcuts::COPY_BYTES;
        use crate::commands::shortcuts::COPY_HEX;
        use crate::commands::shortcuts::QUICK_OPEN;
        use crate::commands::shortcuts::REOPEN_CLOSED_TAB;
        let (close_tab, reopen_tab, copy_bytes, copy_hex, toggle_palette, toggle_quick_open) = ctx.input_mut(|i| {
            (
                i.consume_shortcut(&CLOSE_TAB),
                i.consume_shortcut(&REOPEN_CLOSED_TAB),
                i.consume_shortcut(&COPY_BYTES),
                i.consume_shortcut(&COPY_HEX),
                i.consume_shortcut(&COMMAND_PALETTE),
                i.consume_shortcut(&QUICK_OPEN),
            )
        });
        if toggle_palette {
            if self.palette.is_open() {
                self.palette.close();
            } else {
                self.palette.open_at(crate::commands::palette::Mode::Main);
            }
        }
        if toggle_quick_open {
            if self.palette.is_open() {
                self.palette.close();
            } else {
                self.palette.open_at(crate::commands::palette::Mode::QuickOpen);
            }
        }
        // Shared shortcut handlers: Cmd+Z / Cmd+Shift+Z (undo/redo),
        // Cmd+N (new), Cmd+E (toggle edit), Cmd+V / Cmd+Shift+V
        // (paste / paste-as-hex), Cmd+F (find local) / Cmd+Shift+F
        // (find in all files). Same code paths desktop runs.
        crate::app::shortcuts::dispatch_save_shortcut(&ctx, self);
        crate::app::shortcuts::dispatch_paste_shortcut(&ctx, self);
        crate::app::shortcuts::dispatch_find_shortcut(&ctx, self);
        // Tab-bar focus + cycling: Ctrl+Tab / Ctrl+Shift+Tab cycle
        // tabs in the focused leaf; Alt+Tab toggles inner/outer
        // dock focus; Cmd+K stages the visual focus picker.
        crate::tabs::focus::dispatch_tab_focus_toggle(&ctx, self);
        crate::tabs::focus::dispatch_focus_pane_shortcut(&ctx, self);
        crate::tabs::focus::dispatch_tab_cycle(&ctx, self);
        if close_tab && let Some(id) = self.last_active_file {
            self.close_file_tab_wasm(id);
        }
        if reopen_tab {
            self.reopen_last_closed_wasm();
        }
        if (copy_bytes || copy_hex)
            && let Some(text) = self.copy_active_selection_wasm(copy_hex)
        {
            ctx.copy_text(text);
        }
        // Skip editor input dispatch while the palette is open so the
        // palette gets first crack at Escape / arrow keys / Enter.
        // Without this the editor's Escape handler (selection clear,
        // Vim mode exit) eats the key before egui_palette sees it
        // and the palette can't dismiss.
        if !self.palette.is_open()
            && let Some(id) = self.last_active_file
            && let Some(file) = self.files.get_mut(&id)
        {
            file.editor.handle_input(&ctx);
        }
        egui::Panel::top("hxy_top_bar").show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("New").clicked() {
                    self.open_bytes_wasm("Untitled".to_owned(), Vec::new());
                }
                if ui.button("Open files...").clicked() {
                    let ctx_clone = ctx.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        let Some(handles) = rfd::AsyncFileDialog::new().pick_files().await else { return };
                        for handle in handles {
                            let bytes = handle.read().await;
                            let name = handle.file_name();
                            push_open_request_wasm(name, bytes);
                        }
                        ctx_clone.request_repaint();
                    });
                }
                let snapshot = self.active_file_bytes_wasm();
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
                let active_id = self.last_active_file;
                ui.add_enabled_ui(active_id.is_some(), |ui| {
                    if ui.button("Strings").clicked()
                        && let Some(id) = active_id
                    {
                        self.toggle_tab_wasm(Tab::Strings(id));
                    }
                    if ui.button("Checksums").clicked()
                        && let Some(id) = active_id
                    {
                        self.toggle_tab_wasm(Tab::Checksums(id));
                    }
                    if ui.button("Entropy").clicked()
                        && let Some(id) = active_id
                    {
                        self.toggle_tab_wasm(Tab::Entropy(id));
                    }
                });
                if ui.button("Inspector").clicked() {
                    self.toggle_tab_wasm(Tab::Inspector);
                }
                if ui.button("Memory").clicked() {
                    self.toggle_tab_wasm(Tab::Memory);
                }
                if ui.button("Settings").clicked() {
                    self.toggle_tab_wasm(Tab::Settings);
                }
                ui.label("hxy");
            });
        });
        for (name, bytes) in drain_open_requests_wasm() {
            self.open_bytes_wasm(name, bytes);
        }
        let mut pending_close: Vec<FileId> = Vec::new();
        let mut pending_strings_run: Vec<FileId> = Vec::new();
        let mut pending_strings_jump: Vec<(FileId, u64, u64)> = Vec::new();
        let mut pending_checksums_run: Vec<FileId> = Vec::new();
        let mut pending_checksums_copy: Vec<String> = Vec::new();
        let mut pending_entropy_recompute: Vec<FileId> = Vec::new();
        let mut pending_vfs_opens: Vec<(crate::files::WorkspaceId, String)> = Vec::new();
        egui::CentralPanel::default().show_inside(ui, |ui| {
            let style = crate::style::hxy_dock_style(ui.style());
            let mut state_guard = self.state.write();
            egui_dock::DockArea::new(&mut self.dock).style(style).show_inside(
                ui,
                &mut WasmTabViewer {
                    files: &mut self.files,
                    last_active_file: &mut self.last_active_file,
                    byte_cache: &self.byte_cache,
                    state: &mut state_guard,
                    tab_focus: &mut self.tab_focus,
                    workspaces: &mut self.workspaces,
                    compares: &mut self.compares,
                    global_search: &mut self.global_search,
                    pending_global_search_events: &mut self.pending_global_search_events,
                    pending_vfs_opens: &mut pending_vfs_opens,
                    pending_close: &mut pending_close,
                    pending_strings_run: &mut pending_strings_run,
                    pending_strings_jump: &mut pending_strings_jump,
                    pending_checksums_run: &mut pending_checksums_run,
                    pending_checksums_copy: &mut pending_checksums_copy,
                    pending_entropy_recompute: &mut pending_entropy_recompute,
                },
            );
        });
        for (workspace_id, entry_path) in pending_vfs_opens {
            self.open_vfs_entry_wasm(workspace_id, entry_path);
        }
        for id in pending_close {
            self.close_file_tab_wasm(id);
        }
        for id in pending_strings_run {
            self.spawn_strings_run_wasm(&ctx, id);
        }
        for (id, offset, end) in pending_strings_jump {
            self.jump_to_offset_wasm(id, offset, end);
        }
        for id in pending_checksums_run {
            self.spawn_checksums_run_wasm(&ctx, id);
        }
        for text in pending_checksums_copy {
            ctx.copy_text(text);
        }
        for id in pending_entropy_recompute {
            self.spawn_entropy_run_wasm(&ctx, id);
        }
        self.drain_panel_runs_wasm(&ctx);
        // Render the command palette over the dock when open.
        // Same `egui_palette::show` rendering path the desktop
        // build uses; entries are built inline by
        // `build_wasm_palette_entries` since the desktop's
        // `commands::palette::entries` reaches into too many
        // desktop-only modules (plugin host, watcher, sync rfd).
        if self.palette.is_open() {
            let entries = build_wasm_palette_entries(&ctx, self);
            if let Some(outcome) = crate::commands::palette::show(&ctx, &mut self.palette, entries) {
                self.apply_wasm_palette_outcome(&ctx, outcome);
            }
        }
        // Search side-effects: turn per-file pending_effects (wrapped /
        // replaced / length-mismatch ack / replace-all confirm) into
        // toasts + modals. Same path desktop runs.
        crate::search::modal::drain_search_effects(self);
        crate::search::modal::render_search_modal(&ctx, self);
        // Cross-file search results: drain events emitted by the
        // SearchResults tab, run / refresh / jump / close as needed.
        let global_events = std::mem::take(&mut self.pending_global_search_events);
        if !global_events.is_empty() {
            apply_global_search_events(self, global_events);
        }
        self.toasts.show_toasts(&ctx);
        // Visual pane picker overlay -- same code path desktop uses
        // via `tabs::focus::handle_pane_pick`. That helper lives in a
        // desktop-only module though, so the body is inlined here so
        // wasm doesn't need to ungate `tabs::focus` (which has other
        // desktop-only deps).
        if let Some(pending) = self.pending_pane_pick {
            let whitelist = self.pane_pick_target_paths.clone();
            let outcome = crate::tabs::pane_pick::tick(
                &ctx,
                &self.dock,
                pending,
                &mut self.pane_pick_letters,
                whitelist.as_deref(),
            );
            match outcome {
                crate::tabs::pane_pick::TickOutcome::Continue => {}
                crate::tabs::pane_pick::TickOutcome::Cancel => {
                    self.pending_pane_pick = None;
                    self.pane_pick_target_paths = None;
                }
                crate::tabs::pane_pick::TickOutcome::Picked { source, target, op } => {
                    self.pending_pane_pick = None;
                    self.pane_pick_target_paths = None;
                    match op {
                        crate::tabs::pane_pick::PaneOp::MoveTab => {
                            if let Some(source) = source {
                                crate::tabs::dock_ops::dock_move_tab_to(self, source, target);
                            }
                        }
                        crate::tabs::pane_pick::PaneOp::Merge => {
                            if let Some(source) = source {
                                crate::tabs::dock_ops::dock_merge_to(self, source, target);
                            }
                        }
                        crate::tabs::pane_pick::PaneOp::Focus => {
                            self.dock.set_focused_node_and_surface(target);
                            self.tab_focus = TabFocus::Outer;
                        }
                        crate::tabs::pane_pick::PaneOp::CloseToolLeaf => {
                            crate::tabs::dock_ops::close_tool_leaf(self, target);
                        }
                    }
                }
            }
        }
    }
}

struct WasmTabViewer<'a> {
    files: &'a mut HashMap<FileId, OpenFile>,
    last_active_file: &'a mut Option<FileId>,
    byte_cache: &'a Arc<hxy_core::ByteCache>,
    state: &'a mut PersistedState,
    tab_focus: &'a mut TabFocus,
    workspaces: &'a mut std::collections::BTreeMap<crate::files::WorkspaceId, crate::files::Workspace>,
    compares: &'a mut std::collections::BTreeMap<crate::compare::CompareId, crate::compare::CompareSession>,
    global_search: &'a mut crate::search::global::GlobalSearchState,
    pending_global_search_events: &'a mut Vec<crate::search::global::GlobalSearchEvent>,
    /// Pending VFS-entry opens queued by the workspace's inner VFS
    /// tree this frame. Drained after the dock pass so we can mutate
    /// `files` without holding a borrow into `workspaces`.
    pending_vfs_opens: &'a mut Vec<(crate::files::WorkspaceId, String)>,
    pending_close: &'a mut Vec<FileId>,
    pending_strings_run: &'a mut Vec<FileId>,
    pending_strings_jump: &'a mut Vec<(FileId, u64, u64)>,
    pending_checksums_run: &'a mut Vec<FileId>,
    pending_checksums_copy: &'a mut Vec<String>,
    pending_entropy_recompute: &'a mut Vec<FileId>,
}

fn inspector_window_wasm(file: &OpenFile) -> Option<(hxy_core::ByteOffset, Vec<u8>)> {
    let sel = file.editor.selection()?;
    let caret = sel.cursor;
    let total = file.editor.source().len().get();
    if total == 0 {
        return None;
    }
    let start = caret.get();
    let end = (start + 16).min(total);
    let range = hxy_core::ByteRange::new(caret, hxy_core::ByteOffset::new(end)).ok()?;
    let bytes = file.editor.source().read(range).ok()?;
    Some((caret, bytes))
}

impl egui_dock::TabViewer for WasmTabViewer<'_> {
    type Tab = Tab;

    fn closeable(&mut self, tab: &mut Self::Tab) -> bool {
        matches!(tab, Tab::File(_))
    }

    fn on_close(&mut self, tab: &mut Self::Tab) -> egui_dock::tab_viewer::OnCloseResponse {
        if let Tab::File(id) = tab {
            self.pending_close.push(*id);
            egui_dock::tab_viewer::OnCloseResponse::Ignore
        } else {
            egui_dock::tab_viewer::OnCloseResponse::Close
        }
    }

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        let panel_title = |id: &FileId, label_key: &str| -> egui::WidgetText {
            let name = self.files.get(id).map(|f| f.display_name.as_str()).unwrap_or("(missing)");
            hxy_i18n::t_args(label_key, &[("name", name)]).into()
        };
        match tab {
            Tab::Welcome => hxy_i18n::t("tab-welcome").into(),
            Tab::Settings => hxy_i18n::t("tab-settings").into(),
            Tab::Console => hxy_i18n::t("tab-console").into(),
            Tab::Inspector => hxy_i18n::t("tab-inspector").into(),
            Tab::Plugins => hxy_i18n::t("tab-plugins").into(),
            Tab::Memory => hxy_i18n::t("tab-memory").into(),
            Tab::File(id) => match self.files.get(id) {
                Some(f) => format_file_tab_title(f).into(),
                None => format!("file-{}", id.get()).into(),
            },
            Tab::Workspace(id) => match self.workspaces.get(id) {
                Some(w) => match self.files.get(&w.editor_id) {
                    Some(f) => format_workspace_tab_title(f).into(),
                    None => format!("workspace-{}", id.get()).into(),
                },
                None => format!("workspace-{}", id.get()).into(),
            },
            Tab::Entropy(id) => panel_title(id, "tab-entropy"),
            Tab::Strings(id) => panel_title(id, "tab-strings"),
            Tab::Checksums(id) => panel_title(id, "tab-checksums"),
            Tab::SearchResults => {
                format!("{} {}", egui_phosphor::regular::MAGNIFYING_GLASS, hxy_i18n::t("tab-search-results")).into()
            }
            Tab::Compare(compare_id) => match self.compares.get(compare_id) {
                Some(s) => {
                    hxy_i18n::t_args("tab-compare-title", &[("a", &s.a.display_name), ("b", &s.b.display_name)]).into()
                }
                None => format!("compare-{}", compare_id.get()).into(),
            },
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        match tab {
            Tab::Welcome => welcome_ui(ui, self.state),
            Tab::File(id) => {
                let id = *id;
                if let Some(file) = self.files.get_mut(&id) {
                    *self.last_active_file = Some(id);
                    render_file_tab(ui, id, file, self.state, *self.tab_focus);
                } else {
                    ui.colored_label(egui::Color32::RED, format!("missing file {id:?}"));
                }
            }
            Tab::Settings => {
                settings_ui(ui, &mut self.state.app, self.files, self.byte_cache);
            }
            Tab::Workspace(workspace_id) => {
                let workspace_id = *workspace_id;
                let style = crate::style::hxy_dock_style(ui.style());
                let mut local_pending_close: Vec<FileId> = Vec::new();
                let editor_id = match self.workspaces.get(&workspace_id) {
                    Some(w) => w.editor_id,
                    None => {
                        ui.colored_label(egui::Color32::RED, format!("missing workspace {workspace_id:?}"));
                        return;
                    }
                };
                if let Some(workspace) = self.workspaces.get_mut(&workspace_id) {
                    let mount = workspace.mount.clone();
                    let mut viewer = WasmWorkspaceTabViewer {
                        files: self.files,
                        state: self.state,
                        editor_id,
                        workspace_id,
                        mount: &mount,
                        tab_focus: self.tab_focus,
                        pending_vfs_opens: self.pending_vfs_opens,
                        pending_close: &mut local_pending_close,
                    };
                    egui_dock::DockArea::new(&mut workspace.dock).style(style).show_inside(ui, &mut viewer);
                }
                for id in local_pending_close {
                    self.pending_close.push(id);
                }
            }
            Tab::Inspector => {
                let bytes_for_inspector =
                    self.last_active_file.and_then(|id| self.files.get(&id)).and_then(inspector_window_wasm);
                let (caret, bytes) = match bytes_for_inspector.as_ref() {
                    Some((c, b)) => (Some(c.get()), b.as_slice()),
                    None => (None, &[] as &[u8]),
                };
                let mut state = crate::panels::inspector::InspectorState::default();
                let decoders = crate::panels::inspector::default_decoders();
                crate::panels::inspector::show(ui, &mut state, &decoders, caret, bytes);
            }
            Tab::Strings(file_id) => {
                let pinned = *file_id;
                if let Some(file) = self.files.get_mut(&pinned) {
                    if file.strings_panel.config.range.is_empty() {
                        let len = file.editor.source().len().get();
                        if let Ok(range) =
                            hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(len))
                        {
                            file.strings_panel.config.range = range;
                        }
                    }
                    let label = file.display_name.clone();
                    let events = match file.virtual_base {
                        Some(base) => {
                            crate::panels::strings::show_with_vaddr(ui, Some(&label), &mut file.strings_panel, base)
                        }
                        None => crate::panels::strings::show(ui, Some(&label), &mut file.strings_panel),
                    };
                    for ev in events {
                        match ev {
                            crate::panels::strings::StringsEvent::Run => self.pending_strings_run.push(pinned),
                            crate::panels::strings::StringsEvent::Jump { offset, end } => {
                                self.pending_strings_jump.push((pinned, offset, end));
                            }
                        }
                    }
                } else {
                    ui.colored_label(egui::Color32::RED, format!("missing file {pinned:?}"));
                }
            }
            Tab::Checksums(file_id) => {
                let pinned = *file_id;
                if let Some(file) = self.files.get_mut(&pinned) {
                    if file.checksums_panel.config.range.is_empty() {
                        let len = file.editor.source().len().get();
                        if let Ok(range) =
                            hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(len))
                        {
                            file.checksums_panel.config.range = range;
                        }
                    }
                    let label = file.display_name.clone();
                    let events = match file.virtual_base {
                        Some(base) => {
                            crate::panels::checksums::show_with_vaddr(ui, Some(&label), &mut file.checksums_panel, base)
                        }
                        None => crate::panels::checksums::show(ui, Some(&label), &mut file.checksums_panel),
                    };
                    for ev in events {
                        match ev {
                            crate::panels::checksums::ChecksumsEvent::Run => {
                                self.pending_checksums_run.push(pinned);
                            }
                            crate::panels::checksums::ChecksumsEvent::Copy(text) => {
                                self.pending_checksums_copy.push(text);
                            }
                        }
                    }
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
                if clicked {
                    self.pending_entropy_recompute.push(pinned);
                }
            }
            Tab::Memory => {
                let labels = crate::panels::memory::ViewLabels::from_files(self.files);
                crate::panels::memory::memory_ui(ui, self.byte_cache, &labels);
            }
            Tab::Console => {
                // Wasm has no log writers wired (plugin host /
                // template runner / file watcher are all desktop-
                // only) so the buffer is always empty -- console_ui
                // renders the "no entries yet" placeholder.
                let empty: std::collections::VecDeque<ConsoleEntry> = std::collections::VecDeque::new();
                console_ui(ui, &empty);
            }
            Tab::Plugins => {
                // Plugin manager (browse / install / uninstall) is
                // desktop-only -- wasmtime + filesystem operations
                // don't run in the browser. Surface a stub so the
                // tab doesn't read "(not yet wired)".
                ui.vertical_centered(|ui| {
                    ui.add_space(24.0);
                    ui.heading(hxy_i18n::t("tab-plugins"));
                    ui.add_space(8.0);
                    ui.weak(hxy_i18n::t("plugins-wasm-unavailable"));
                });
            }
            Tab::Compare(compare_id) => match self.compares.get_mut(compare_id) {
                Some(session) => crate::compare::tab::render_compare_tab(ui, session, self.state),
                None => {
                    ui.colored_label(egui::Color32::RED, format!("missing compare {compare_id:?}"));
                }
            },
            Tab::SearchResults => {
                let names: std::collections::HashMap<FileId, String> =
                    self.files.iter().map(|(id, f)| (*id, f.display_name.clone())).collect();
                let events = crate::search::global::show(ui, self.global_search, &names);
                self.pending_global_search_events.extend(events);
            }
        }
    }
}

struct WasmWorkspaceTabViewer<'a> {
    files: &'a mut HashMap<FileId, OpenFile>,
    state: &'a mut PersistedState,
    editor_id: FileId,
    workspace_id: crate::files::WorkspaceId,
    mount: &'a Arc<MountedVfs>,
    tab_focus: &'a mut TabFocus,
    pending_vfs_opens: &'a mut Vec<(crate::files::WorkspaceId, String)>,
    pending_close: &'a mut Vec<FileId>,
}

impl egui_dock::TabViewer for WasmWorkspaceTabViewer<'_> {
    type Tab = crate::files::WorkspaceTab;

    fn closeable(&mut self, tab: &mut Self::Tab) -> bool {
        matches!(tab, crate::files::WorkspaceTab::Entry(_) | crate::files::WorkspaceTab::VfsTree)
    }

    fn on_close(&mut self, tab: &mut Self::Tab) -> egui_dock::tab_viewer::OnCloseResponse {
        if let crate::files::WorkspaceTab::Entry(id) = tab {
            self.pending_close.push(*id);
            egui_dock::tab_viewer::OnCloseResponse::Ignore
        } else {
            egui_dock::tab_viewer::OnCloseResponse::Close
        }
    }

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        match tab {
            crate::files::WorkspaceTab::Editor => match self.files.get(&self.editor_id) {
                Some(f) => f.display_name.clone().into(),
                None => format!("editor-{}", self.editor_id.get()).into(),
            },
            crate::files::WorkspaceTab::VfsTree => "VFS".into(),
            crate::files::WorkspaceTab::Entry(id) => match self.files.get(id) {
                Some(f) => f.display_name.clone().into(),
                None => format!("entry-{}", id.get()).into(),
            },
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        match tab {
            crate::files::WorkspaceTab::Editor => match self.files.get_mut(&self.editor_id) {
                Some(file) => render_file_tab(ui, self.editor_id, file, self.state, *self.tab_focus),
                None => {
                    ui.colored_label(egui::Color32::RED, format!("missing editor {:?}", self.editor_id));
                }
            },
            crate::files::WorkspaceTab::VfsTree => {
                let scope = egui::Id::new(("hxy-workspace-vfs-wasm", self.workspace_id.get()));
                let parent_source = self.files.get(&self.editor_id).and_then(|f| f.source_kind.clone());
                let mut scratch: Vec<String> = Vec::new();
                let expanded: &mut Vec<String> = match parent_source.as_ref() {
                    Some(key) => vfs_expanded_for(&mut self.state.vfs_tree_expanded, key),
                    None => &mut scratch,
                };
                let events = crate::panels::vfs::show(ui, scope, &*self.mount.fs, expanded);
                for ev in events {
                    let crate::panels::vfs::VfsPanelEvent::OpenEntry(path) = ev;
                    self.pending_vfs_opens.push((self.workspace_id, path));
                }
            }
            crate::files::WorkspaceTab::Entry(file_id) => match self.files.get_mut(file_id) {
                Some(file) => render_file_tab(ui, *file_id, file, self.state, *self.tab_focus),
                None => {
                    ui.colored_label(egui::Color32::RED, format!("missing entry {file_id:?}"));
                }
            },
        }
    }
}

/// Build a fresh CompareSession from two TabSources (which may be
/// real on-disk paths on desktop or `/__memory__/{id}` synthetics
/// on wasm) and push it as a new `Tab::Compare`. Wasm-only since
/// only synthetic memory paths are supported here -- the desktop
/// path goes through `compare::picker::spawn_compare_from_palette`
/// which also handles real filesystem reads.
fn spawn_compare_wasm(app: &mut HxyApp, a: TabSource, b: TabSource) {
    let Some((a_name, a_bytes)) = app.read_compare_side_wasm(&a) else { return };
    let Some((b_name, b_bytes)) = app.read_compare_side_wasm(&b) else { return };
    let id = crate::compare::CompareId::new(app.next_compare_id);
    app.next_compare_id += 1;
    let pane_a = crate::compare::ComparePane::from_bytes(a_name, Some(a), a_bytes);
    let pane_b = crate::compare::ComparePane::from_bytes(b_name, Some(b), b_bytes);
    let session = crate::compare::CompareSession::new(id, pane_a, pane_b);
    app.compares.insert(id, session);
    app.dock.push_to_focused_leaf(Tab::Compare(id));
    if let Some(path) = app.dock.find_tab(&Tab::Compare(id)) {
        let _ = app.dock.set_active_tab(path);
    }
}

type OpenRequestWasm = (String, Vec<u8>);

thread_local! {
    static OPEN_INBOX_WASM: std::cell::RefCell<Vec<OpenRequestWasm>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn push_open_request_wasm(name: String, bytes: Vec<u8>) {
    OPEN_INBOX_WASM.with(|q| q.borrow_mut().push((name, bytes)));
}

fn drain_open_requests_wasm() -> Vec<OpenRequestWasm> {
    OPEN_INBOX_WASM.with(|q| std::mem::take(&mut *q.borrow_mut()))
}

/// Build the wasm-side command-palette entry list. Mirrors a
/// subset of the desktop's `crate::commands::palette::entries`
/// builder using the SAME `Entry` / `Action` / `PaletteCommand`
/// types so palette rendering is one code path. Entries that
/// reach into the desktop-only ungated dispatch (plugin host,
/// templates runner, file watcher, sync rfd) are dropped.
fn build_wasm_palette_entries(
    ctx: &egui::Context,
    app: &HxyApp,
) -> Vec<egui_palette::Entry<crate::commands::palette::Action>> {
    use crate::commands::palette::Action;
    use crate::commands::palette::Mode;
    use crate::commands::palette::PaletteCommand;
    // Argument-style modes share their entry builders with desktop.
    // They each take a query + a resolver and emit a single dynamic
    // entry. Wasm uses NullResolver since it has no plugin-supplied
    // template fields to resolve `name.length`-style paths against.
    let resolver = hxy_calculator::NullResolver;
    let query = app.palette.inner.query.trim();
    let offset_ctx = match app.last_active_file.and_then(|id| app.files.get(&id)) {
        Some(file) => {
            let source_len = file.editor.source().len().get();
            let sel = file.editor.selection();
            let cursor = sel.map(|s| s.cursor.get()).unwrap_or(0);
            let selection = sel.map(|s| {
                let r = s.range();
                (r.start().get(), r.end().get())
            });
            crate::commands::palette::offset::OffsetPaletteContext {
                cursor,
                source_len,
                available: true,
                selection,
                virtual_base: file.virtual_base,
            }
        }
        None => crate::commands::palette::offset::OffsetPaletteContext::default(),
    };
    // Mode::Main handles two query-prefix shortcuts inline before
    // building the standard list:
    //   @<expr> -> evaluate as offset, jump caret
    //   =<expr> -> evaluate as expression, copy result (decimal+hex)
    // Same builders desktop uses, same Action types -- no fragmentation.
    if matches!(app.palette.mode, Mode::Main) {
        let trimmed = app.palette.inner.query.trim_start();
        if let Some(rest) = trimmed.strip_prefix('@') {
            let mut out = Vec::new();
            crate::commands::palette::build_calculator_entry(&mut out, rest, &offset_ctx, &resolver);
            return out;
        }
        if let Some(rest) = trimmed.strip_prefix('=') {
            let mut out = Vec::new();
            crate::commands::palette::build_calculator_copy_entries(&mut out, rest, &resolver);
            return out;
        }
    }
    if matches!(app.palette.mode, Mode::QuickOpen) {
        let mut out = Vec::new();
        for (id, file) in app.files.iter() {
            out.push(
                egui_palette::Entry::new(file.display_name.clone(), Action::FocusFile(*id))
                    .with_icon(egui_phosphor::regular::FILE),
            );
        }
        return out;
    }
    if !matches!(app.palette.mode, Mode::Main | Mode::QuickOpen) {
        let mut out = Vec::new();
        match app.palette.mode {
            Mode::SetVirtualBase => {
                if !offset_ctx.available {
                    crate::commands::palette::invalid_entry(
                        &mut out,
                        query,
                        &hxy_i18n::t("palette-invalid-no-active-file"),
                    );
                } else {
                    crate::commands::palette::build_virtual_base_entries(&mut out, query, &resolver);
                }
            }
            Mode::GoToOffset | Mode::GoToAddress | Mode::SelectFromOffset | Mode::SelectRange => {
                if !offset_ctx.available {
                    crate::commands::palette::invalid_entry(
                        &mut out,
                        query,
                        &hxy_i18n::t("palette-invalid-no-active-file"),
                    );
                } else {
                    crate::commands::palette::offset::build_offset_entries(
                        &mut out,
                        app.palette.mode,
                        query,
                        &offset_ctx,
                        &resolver,
                    );
                }
            }
            Mode::SetColumnsLocal | Mode::SetColumnsGlobal => {
                if matches!(app.palette.mode, Mode::SetColumnsLocal) && !offset_ctx.available {
                    crate::commands::palette::invalid_entry(
                        &mut out,
                        query,
                        &hxy_i18n::t("palette-invalid-no-active-file"),
                    );
                } else {
                    crate::commands::palette::columns::build_columns_entries(
                        &mut out,
                        app.palette.mode,
                        query,
                        &resolver,
                    );
                }
            }
            _ => {}
        }
        return out;
    }
    let fmt = |sc: &egui::KeyboardShortcut| ctx.format_shortcut(sc);
    let cmd_n = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::N);
    let cmd_f = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::F);
    let cmd_e = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::E);
    let cmd_shift_t = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT), egui::Key::T);
    let cmd_c = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::C);
    let cmd_shift_c = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT), egui::Key::C);
    let has_active = app.last_active_file.is_some();
    let has_closed = !app.closed_tabs.is_empty();
    let has_selection = app
        .last_active_file
        .and_then(|id| app.files.get(&id))
        .and_then(|f| f.editor.selection())
        .is_some_and(|s| !s.range().is_empty());
    let active_vbase = app.last_active_file.and_then(|id| app.files.get(&id)).and_then(|f| f.virtual_base);
    let mut out: Vec<egui_palette::Entry<Action>> = vec![
        egui_palette::Entry::new(hxy_i18n::t("menu-file-new"), Action::InvokeCommand(PaletteCommand::NewFile))
            .with_shortcut(fmt(&cmd_n)),
        egui_palette::Entry::new(hxy_i18n::t("toolbar-open-file"), Action::InvokeCommand(PaletteCommand::OpenFile)),
        egui_palette::Entry::new(
            hxy_i18n::t("palette-save-as-download"),
            Action::InvokeCommand(PaletteCommand::SaveAsDownload),
        )
        .with_icon(icon::DOWNLOAD)
        .with_disabled(!has_active),
        egui_palette::Entry::new(
            hxy_i18n::t("menu-file-reopen-closed"),
            Action::InvokeCommand(PaletteCommand::ReopenClosedTab),
        )
        .with_shortcut(fmt(&cmd_shift_t))
        .with_disabled(!has_closed),
        egui_palette::Entry::new(
            hxy_i18n::t("palette-toggle-readonly"),
            Action::InvokeCommand(PaletteCommand::ToggleEditMode),
        )
        .with_shortcut(fmt(&cmd_e))
        .with_disabled(!has_active),
        egui_palette::Entry::new("Toggle search bar", Action::InvokeCommand(PaletteCommand::FindStringsWholeFile))
            .with_shortcut(fmt(&cmd_f))
            .with_disabled(!has_active),
        egui_palette::Entry::new(
            hxy_i18n::t("palette-copy-caret-offset"),
            Action::InvokeCommand(PaletteCommand::CopyCaretOffset),
        )
        .with_shortcut(fmt(&cmd_c))
        .with_disabled(!has_active),
    ];
    if has_selection {
        out.push(
            egui_palette::Entry::new(
                hxy_i18n::t("palette-copy-selection-range"),
                Action::InvokeCommand(PaletteCommand::CopySelectionRange),
            )
            .with_shortcut(fmt(&cmd_shift_c)),
        );
    }
    use egui_phosphor::regular as icon;
    out.extend([
        egui_palette::Entry::new(
            hxy_i18n::t("palette-strings-whole-file"),
            Action::InvokeCommand(PaletteCommand::FindStringsWholeFile),
        )
        .with_icon(icon::TEXT_AA)
        .with_disabled(!has_active),
        egui_palette::Entry::new(
            hxy_i18n::t("palette-checksums-whole-file"),
            Action::InvokeCommand(PaletteCommand::CalculateChecksumsWholeFile),
        )
        .with_icon(icon::FINGERPRINT)
        .with_disabled(!has_active),
        egui_palette::Entry::new(
            hxy_i18n::t("palette-compute-entropy"),
            Action::InvokeCommand(PaletteCommand::ComputeEntropy),
        )
        .with_icon(icon::CHART_LINE)
        .with_disabled(!has_active),
        egui_palette::Entry::new(
            hxy_i18n::t("palette-tool-show-inspector"),
            Action::InvokeCommand(PaletteCommand::ToggleInspector),
        )
        .with_icon(icon::MAGNIFYING_GLASS),
        egui_palette::Entry::new(
            hxy_i18n::t("palette-tool-show-memory"),
            Action::InvokeCommand(PaletteCommand::ToggleMemory),
        )
        .with_icon(icon::MEMORY),
        egui_palette::Entry::new(
            hxy_i18n::t("palette-tool-show-console"),
            Action::InvokeCommand(PaletteCommand::ToggleConsole),
        )
        .with_icon(icon::TERMINAL_WINDOW),
        egui_palette::Entry::new(
            hxy_i18n::t("palette-tool-show-settings"),
            Action::InvokeCommand(PaletteCommand::ToggleSettings),
        )
        .with_icon(icon::GEAR),
        egui_palette::Entry::new(hxy_i18n::t("palette-toggle-vim"), Action::InvokeCommand(PaletteCommand::ToggleVim))
            .with_icon(icon::KEYBOARD),
        egui_palette::Entry::new(
            hxy_i18n::t("palette-compare-files"),
            Action::InvokeCommand(PaletteCommand::CompareFiles),
        )
        .with_icon(icon::ARROWS_LEFT_RIGHT)
        .with_disabled(app.files.len() < 2),
    ]);
    // Editing: undo/redo and paste variants. Wired through the
    // shared shortcut helpers; same dispatch as desktop palette.
    use crate::commands::shortcuts::PASTE;
    use crate::commands::shortcuts::PASTE_AS_HEX;
    use crate::commands::shortcuts::REDO;
    use crate::commands::shortcuts::UNDO;
    out.extend([
        egui_palette::Entry::new(hxy_i18n::t("palette-undo"), Action::InvokeCommand(PaletteCommand::Undo))
            .with_shortcut(fmt(&UNDO))
            .with_icon(icon::ARROW_COUNTER_CLOCKWISE)
            .with_disabled(!has_active),
        egui_palette::Entry::new(hxy_i18n::t("palette-redo"), Action::InvokeCommand(PaletteCommand::Redo))
            .with_shortcut(fmt(&REDO))
            .with_icon(icon::ARROW_CLOCKWISE)
            .with_disabled(!has_active),
        egui_palette::Entry::new(hxy_i18n::t("palette-paste"), Action::InvokeCommand(PaletteCommand::Paste))
            .with_shortcut(fmt(&PASTE))
            .with_icon(icon::CLIPBOARD)
            .with_disabled(!has_active),
        egui_palette::Entry::new(
            hxy_i18n::t("palette-paste-as-hex"),
            Action::InvokeCommand(PaletteCommand::PasteAsHex),
        )
        .with_shortcut(fmt(&PASTE_AS_HEX))
        .with_icon(icon::CLIPBOARD_TEXT)
        .with_disabled(!has_active),
    ]);
    // Selection-scoped strings / checksums (in addition to whole-
    // file already in the list above).
    if has_selection {
        out.extend([
            egui_palette::Entry::new(
                hxy_i18n::t("palette-strings-selection"),
                Action::InvokeCommand(PaletteCommand::FindStringsSelection),
            )
            .with_icon(icon::TEXT_AA),
            egui_palette::Entry::new(
                hxy_i18n::t("palette-checksums-selection"),
                Action::InvokeCommand(PaletteCommand::CalculateChecksumsSelection),
            )
            .with_icon(icon::FINGERPRINT),
        ]);
    }
    // BrowseVfs / ToggleWorkspaceVfs -- only shown when meaningful.
    let has_handler =
        app.last_active_file.and_then(|id| app.files.get(&id)).is_some_and(|f| f.detected_handler.is_some());
    if has_handler {
        out.push(
            egui_palette::Entry::new(
                hxy_i18n::t("palette-browse-vfs"),
                Action::InvokeCommand(PaletteCommand::BrowseVfs),
            )
            .with_icon(icon::TREE_STRUCTURE),
        );
    }
    if !app.workspaces.is_empty() {
        out.push(
            egui_palette::Entry::new(
                hxy_i18n::t("palette-toggle-workspace-vfs"),
                Action::InvokeCommand(PaletteCommand::ToggleWorkspaceVfs),
            )
            .with_icon(icon::TREE_STRUCTURE),
        );
    }
    if has_active {
        out.push(
            egui_palette::Entry::new(hxy_i18n::t("palette-go-to-offset-entry"), Action::SwitchMode(Mode::GoToOffset))
                .with_icon(icon::CROSSHAIR),
        );
        if active_vbase.is_some() {
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-go-to-address-entry"),
                    Action::SwitchMode(Mode::GoToAddress),
                )
                .with_icon(icon::CROSSHAIR_SIMPLE),
            );
        }
        out.push(
            egui_palette::Entry::new(
                hxy_i18n::t("palette-select-from-offset-entry"),
                Action::SwitchMode(Mode::SelectFromOffset),
            )
            .with_icon(icon::ARROWS_OUT_LINE_HORIZONTAL),
        );
        out.push(
            egui_palette::Entry::new(hxy_i18n::t("palette-select-range-entry"), Action::SwitchMode(Mode::SelectRange))
                .with_icon(icon::BRACKETS_CURLY),
        );
        out.push(
            egui_palette::Entry::new(
                hxy_i18n::t("palette-set-columns-local-entry"),
                Action::SwitchMode(Mode::SetColumnsLocal),
            )
            .with_icon(icon::COLUMNS),
        );
        out.push(
            egui_palette::Entry::new(
                hxy_i18n::t("palette-copy-file-length"),
                Action::InvokeCommand(PaletteCommand::CopyFileLength),
            )
            .with_icon(icon::RULER),
        );
        if has_selection {
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-copy-selection-length"),
                    Action::InvokeCommand(PaletteCommand::CopySelectionLength),
                )
                .with_icon(icon::RULER),
            );
        }
    }
    out.push(
        egui_palette::Entry::new(
            hxy_i18n::t("palette-set-columns-global-entry"),
            Action::SwitchMode(Mode::SetColumnsGlobal),
        )
        .with_icon(icon::COLUMNS_PLUS_RIGHT),
    );
    let vbase_label = match active_vbase {
        Some(addr) => {
            hxy_i18n::t_args("palette-set-virtual-base-entry-current", &[("address", &format!("0x{addr:X}"))])
        }
        None => hxy_i18n::t("palette-set-virtual-base-entry"),
    };
    out.push(
        egui_palette::Entry::new(vbase_label, Action::SwitchMode(Mode::SetVirtualBase))
            .with_icon(icon::TARGET)
            .with_disabled(!has_active),
    );
    // Dock pane management -- universal dock_ops module powers all of
    // these on both targets.
    out.extend([
        egui_palette::Entry::new(hxy_i18n::t("palette-split-right"), Action::InvokeCommand(PaletteCommand::SplitRight))
            .with_icon(icon::ARROW_LINE_RIGHT),
        egui_palette::Entry::new(hxy_i18n::t("palette-split-left"), Action::InvokeCommand(PaletteCommand::SplitLeft))
            .with_icon(icon::ARROW_LINE_LEFT),
        egui_palette::Entry::new(hxy_i18n::t("palette-split-up"), Action::InvokeCommand(PaletteCommand::SplitUp))
            .with_icon(icon::ARROW_LINE_UP),
        egui_palette::Entry::new(hxy_i18n::t("palette-split-down"), Action::InvokeCommand(PaletteCommand::SplitDown))
            .with_icon(icon::ARROW_LINE_DOWN),
        egui_palette::Entry::new(hxy_i18n::t("palette-merge-right"), Action::InvokeCommand(PaletteCommand::MergeRight))
            .with_icon(icon::ARROW_BEND_DOWN_RIGHT),
        egui_palette::Entry::new(hxy_i18n::t("palette-merge-left"), Action::InvokeCommand(PaletteCommand::MergeLeft))
            .with_icon(icon::ARROW_BEND_DOWN_LEFT),
        egui_palette::Entry::new(hxy_i18n::t("palette-merge-up"), Action::InvokeCommand(PaletteCommand::MergeUp))
            .with_icon(icon::ARROW_BEND_UP_RIGHT),
        egui_palette::Entry::new(hxy_i18n::t("palette-merge-down"), Action::InvokeCommand(PaletteCommand::MergeDown))
            .with_icon(icon::ARROW_BEND_UP_RIGHT),
        egui_palette::Entry::new(
            hxy_i18n::t("palette-move-tab-right"),
            Action::InvokeCommand(PaletteCommand::MoveTabRight),
        )
        .with_icon(icon::ARROW_RIGHT),
        egui_palette::Entry::new(
            hxy_i18n::t("palette-move-tab-left"),
            Action::InvokeCommand(PaletteCommand::MoveTabLeft),
        )
        .with_icon(icon::ARROW_LEFT),
        egui_palette::Entry::new(hxy_i18n::t("palette-move-tab-up"), Action::InvokeCommand(PaletteCommand::MoveTabUp))
            .with_icon(icon::ARROW_UP),
        egui_palette::Entry::new(
            hxy_i18n::t("palette-move-tab-down"),
            Action::InvokeCommand(PaletteCommand::MoveTabDown),
        )
        .with_icon(icon::ARROW_DOWN),
        egui_palette::Entry::new(
            hxy_i18n::t("palette-move-tab-visual"),
            Action::InvokeCommand(PaletteCommand::MoveTabVisual),
        )
        .with_icon(icon::ARROWS_OUT_CARDINAL),
        egui_palette::Entry::new(
            hxy_i18n::t("palette-merge-visual"),
            Action::InvokeCommand(PaletteCommand::MergeVisual),
        )
        .with_icon(icon::ARROWS_IN_CARDINAL),
        egui_palette::Entry::new(hxy_i18n::t("palette-focus-pane"), Action::InvokeCommand(PaletteCommand::FocusPane))
            .with_icon(icon::CROSSHAIR),
        egui_palette::Entry::new(
            hxy_i18n::t("palette-close-tool-pane"),
            Action::InvokeCommand(PaletteCommand::CloseToolPane),
        )
        .with_icon(icon::X_SQUARE),
    ]);
    out
}

impl HxyApp {
    /// Dispatch a palette pick on wasm. Mirrors a subset of
    /// `crate::commands::palette::apply::apply_palette_action`
    /// using the SAME `Action` / `PaletteCommand` types --
    /// commands the wasm UI doesn't surface (templates,
    /// plugins, file watcher, sync rfd) are intentionally
    /// dropped here as no-ops.
    fn apply_wasm_palette_outcome(&mut self, ctx: &egui::Context, outcome: crate::commands::palette::Outcome) {
        use crate::commands::palette::Action;
        use crate::commands::palette::Outcome;
        use crate::commands::palette::PaletteCommand;
        let action = match outcome {
            Outcome::Picked(a) => a,
            Outcome::Dismissed(_) => {
                self.palette.close();
                return;
            }
        };
        match action {
            Action::SwitchMode(mode) => {
                self.palette.open_at(mode);
                return;
            }
            Action::SetVirtualBase(addr) => {
                self.palette.close();
                crate::commands::palette::apply_set_virtual_base(self, addr);
                return;
            }
            _ => {}
        }
        self.palette.close();
        match action {
            Action::InvokeCommand(cmd) => match cmd {
                PaletteCommand::NewFile => {
                    self.open_bytes_wasm("Untitled".to_owned(), Vec::new());
                }
                PaletteCommand::OpenFile => {
                    let ctx_clone = ctx.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        let Some(handles) = rfd::AsyncFileDialog::new().pick_files().await else { return };
                        for handle in handles {
                            let bytes = handle.read().await;
                            let name = handle.file_name();
                            push_open_request_wasm(name, bytes);
                        }
                        ctx_clone.request_repaint();
                    });
                }
                PaletteCommand::SaveAsDownload => {
                    if let Some((name, bytes)) = self.active_file_bytes_wasm() {
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
                }
                PaletteCommand::ReopenClosedTab => self.reopen_last_closed_wasm(),
                PaletteCommand::ToggleEditMode => {
                    if let Some(id) = self.last_active_file
                        && let Some(file) = self.files.get_mut(&id)
                    {
                        let next = match file.editor.edit_mode() {
                            crate::files::EditMode::Mutable => crate::files::EditMode::Readonly,
                            crate::files::EditMode::Readonly => crate::files::EditMode::Mutable,
                        };
                        file.editor.set_edit_mode(next);
                    }
                }
                PaletteCommand::FindStringsWholeFile => {
                    if let Some(id) = self.last_active_file {
                        self.toggle_tab_wasm(Tab::Strings(id));
                    }
                }
                PaletteCommand::CalculateChecksumsWholeFile => {
                    if let Some(id) = self.last_active_file {
                        self.toggle_tab_wasm(Tab::Checksums(id));
                    }
                }
                PaletteCommand::ComputeEntropy => {
                    if let Some(id) = self.last_active_file {
                        self.toggle_tab_wasm(Tab::Entropy(id));
                    }
                }
                PaletteCommand::ToggleInspector => {
                    self.toggle_tab_wasm(Tab::Inspector);
                }
                PaletteCommand::CopyCaretOffset => {
                    if let Some(id) = self.last_active_file
                        && let Some(file) = self.files.get(&id)
                        && let Some(sel) = file.editor.selection()
                    {
                        let base = self.state.read().app.offset_base;
                        let text = crate::view::format::format_offset(sel.cursor.get(), base);
                        ctx.copy_text(text);
                    }
                }
                PaletteCommand::CopySelectionRange => {
                    if let Some(text) = self.copy_active_selection_wasm(false) {
                        ctx.copy_text(text);
                    }
                }
                PaletteCommand::CopyCaretAddress => {
                    if let Some(id) = self.last_active_file
                        && let Some(file) = self.files.get(&id)
                        && let Some(sel) = file.editor.selection()
                        && let Some(vaddr) = file.virtual_base
                    {
                        let base = self.state.read().app.offset_base;
                        let text = crate::view::format::format_offset_with_vaddr(sel.cursor.get(), base, vaddr);
                        ctx.copy_text(text);
                    }
                }
                PaletteCommand::CopySelectionLength => {
                    if let Some(id) = self.last_active_file
                        && let Some(file) = self.files.get(&id)
                        && let Some(sel) = file.editor.selection()
                    {
                        let base = self.state.read().app.offset_base;
                        let text = crate::view::format::format_offset(sel.range().len().get(), base);
                        ctx.copy_text(text);
                    }
                }
                PaletteCommand::CopyFileLength => {
                    if let Some(id) = self.last_active_file
                        && let Some(file) = self.files.get(&id)
                    {
                        let base = self.state.read().app.offset_base;
                        let text = crate::view::format::format_offset(file.editor.source().len().get(), base);
                        ctx.copy_text(text);
                    }
                }
                PaletteCommand::ToggleConsole => self.toggle_tab_wasm(Tab::Console),
                PaletteCommand::ToggleSettings => self.toggle_tab_wasm(Tab::Settings),
                PaletteCommand::ToggleMemory => self.toggle_tab_wasm(Tab::Memory),
                PaletteCommand::ToggleVim => crate::app::toggle_vim_mode(self),
                PaletteCommand::CompareFiles => {
                    self.palette.open_at(crate::commands::palette::Mode::CompareSideA);
                }
                PaletteCommand::Undo => crate::app::undo_active_file(self),
                PaletteCommand::Redo => crate::app::redo_active_file(self),
                PaletteCommand::Paste => crate::app::paste_active_file(self, false),
                PaletteCommand::PasteAsHex => crate::app::paste_active_file(self, true),
                PaletteCommand::JumpNextField => crate::app::jump_to_template_field(self, true),
                PaletteCommand::JumpPrevField => crate::app::jump_to_template_field(self, false),
                PaletteCommand::FindStringsSelection => {
                    if let Some(id) = self.last_active_file {
                        if let Some(file) = self.files.get_mut(&id)
                            && let Some(sel) = file.editor.selection()
                        {
                            file.strings_panel.config.range = sel.range();
                        }
                        self.toggle_tab_wasm(Tab::Strings(id));
                    }
                }
                PaletteCommand::CalculateChecksumsSelection => {
                    if let Some(id) = self.last_active_file {
                        if let Some(file) = self.files.get_mut(&id)
                            && let Some(sel) = file.editor.selection()
                        {
                            file.checksums_panel.config.range = sel.range();
                        }
                        self.toggle_tab_wasm(Tab::Checksums(id));
                    }
                }
                PaletteCommand::FindStringsWithOptions => {
                    if let Some(id) = self.last_active_file {
                        self.toggle_tab_wasm(Tab::Strings(id));
                    }
                }
                PaletteCommand::ShowEntropy => {
                    if let Some(id) = self.last_active_file {
                        self.toggle_tab_wasm(Tab::Entropy(id));
                    }
                }
                PaletteCommand::ToggleEntropy => {
                    if let Some(id) = self.last_active_file {
                        self.toggle_tab_wasm(Tab::Entropy(id));
                    }
                }
                PaletteCommand::BrowseVfs => {
                    if let Some(id) = self.last_active_file
                        && self.try_push_as_workspace(id)
                        && let Some(path) = self.dock.find_tab(&Tab::File(id))
                    {
                        let _ = self.dock.remove_tab(path);
                    }
                }
                PaletteCommand::ToggleWorkspaceVfs => {
                    if let Some(workspace_id) = crate::app::active_workspace_id(self)
                        && let Some(workspace) = self.workspaces.get_mut(&workspace_id)
                    {
                        let already = workspace
                            .dock
                            .iter_all_tabs()
                            .any(|(_, t)| matches!(t, crate::files::WorkspaceTab::VfsTree));
                        if already {
                            if let Some(path) = workspace.dock.find_tab(&crate::files::WorkspaceTab::VfsTree) {
                                let _ = workspace.dock.remove_tab(path);
                            }
                        } else {
                            workspace.dock.main_surface_mut().split_left(
                                egui_dock::NodeIndex::root(),
                                0.3,
                                vec![crate::files::WorkspaceTab::VfsTree],
                            );
                        }
                    }
                }
                PaletteCommand::SplitRight => {
                    crate::tabs::dock_ops::dock_split_focused(self, crate::commands::DockDir::Right)
                }
                PaletteCommand::SplitLeft => {
                    crate::tabs::dock_ops::dock_split_focused(self, crate::commands::DockDir::Left)
                }
                PaletteCommand::SplitUp => {
                    crate::tabs::dock_ops::dock_split_focused(self, crate::commands::DockDir::Up)
                }
                PaletteCommand::SplitDown => {
                    crate::tabs::dock_ops::dock_split_focused(self, crate::commands::DockDir::Down)
                }
                PaletteCommand::MergeRight => {
                    crate::tabs::dock_ops::dock_merge_focused(self, crate::commands::DockDir::Right)
                }
                PaletteCommand::MergeLeft => {
                    crate::tabs::dock_ops::dock_merge_focused(self, crate::commands::DockDir::Left)
                }
                PaletteCommand::MergeUp => {
                    crate::tabs::dock_ops::dock_merge_focused(self, crate::commands::DockDir::Up)
                }
                PaletteCommand::MergeDown => {
                    crate::tabs::dock_ops::dock_merge_focused(self, crate::commands::DockDir::Down)
                }
                PaletteCommand::MoveTabRight => {
                    crate::tabs::dock_ops::dock_move_focused_tab(self, crate::commands::DockDir::Right)
                }
                PaletteCommand::MoveTabLeft => {
                    crate::tabs::dock_ops::dock_move_focused_tab(self, crate::commands::DockDir::Left)
                }
                PaletteCommand::MoveTabUp => {
                    crate::tabs::dock_ops::dock_move_focused_tab(self, crate::commands::DockDir::Up)
                }
                PaletteCommand::MoveTabDown => {
                    crate::tabs::dock_ops::dock_move_focused_tab(self, crate::commands::DockDir::Down)
                }
                PaletteCommand::MoveTabVisual => {
                    crate::app::start_pane_pick(self, crate::tabs::pane_pick::PaneOp::MoveTab)
                }
                PaletteCommand::MergeVisual => crate::app::start_pane_pick(self, crate::tabs::pane_pick::PaneOp::Merge),
                PaletteCommand::FocusPane => crate::app::start_pane_focus(self),
                PaletteCommand::CloseToolPane => crate::app::close_tool_pane(self),
                _ => {}
            },
            Action::FocusFile(id) => {
                if let Some(path) = self.dock.find_tab(&Tab::File(id)) {
                    let _ = self.dock.set_active_tab(path);
                }
                self.last_active_file = Some(id);
            }
            Action::FocusTab(tab) => {
                self.toggle_tab_wasm(tab);
            }
            Action::GoToOffset(target) => {
                if let Some(id) = self.last_active_file
                    && let Some(file) = self.files.get_mut(&id)
                {
                    let total = file.editor.source().len().get();
                    let clamped = target.min(total.saturating_sub(1));
                    let anchor = hxy_core::ByteOffset::new(clamped);
                    file.editor.set_selection(Some(hxy_core::Selection { anchor, cursor: anchor }));
                    if !file.editor.is_offset_visible(anchor) {
                        file.editor.set_scroll_to_byte(anchor);
                    }
                }
            }
            Action::SetSelection { start, end_exclusive } => {
                if let Some(id) = self.last_active_file
                    && let Some(file) = self.files.get_mut(&id)
                    && end_exclusive > start
                {
                    let total = file.editor.source().len().get();
                    let s = start.min(total);
                    let e = end_exclusive.min(total).saturating_sub(1).max(s);
                    let anchor = hxy_core::ByteOffset::new(s);
                    let cursor = hxy_core::ByteOffset::new(e);
                    file.editor.set_selection(Some(hxy_core::Selection { anchor, cursor }));
                    if !file.editor.is_offset_visible(anchor) {
                        file.editor.set_scroll_to_byte(anchor);
                    }
                }
            }
            Action::SetColumns { scope, count } => match scope {
                crate::commands::palette::ColumnScope::Local => {
                    if let Some(id) = self.last_active_file
                        && let Some(file) = self.files.get_mut(&id)
                    {
                        file.hex_columns_override = Some(count);
                    }
                }
                crate::commands::palette::ColumnScope::Global => {
                    self.state.write().app.hex_columns = count;
                }
            },
            Action::CompareSelectSource { side, source } => match side {
                crate::commands::palette::CompareSide::A => {
                    self.palette.compare_pick =
                        Some(crate::commands::palette::ComparePickState { picked_a: Some(source) });
                    self.palette.open_at(crate::commands::palette::Mode::CompareSideB);
                }
                crate::commands::palette::CompareSide::B => {
                    let Some(pick) = self.palette.compare_pick.take() else { return };
                    let Some(a) = pick.picked_a else { return };
                    self.palette.close();
                    spawn_compare_wasm(self, a, source);
                }
            },
            _ => {}
        }
    }
}
