//! Main application type.

use std::collections::HashMap;
use std::sync::Arc;

use egui_dock::DockArea;
use egui_dock::DockState;
use egui_dock::Style;
use egui_dock::TabViewer;
use egui_dock::tab_viewer::OnCloseResponse;
use hxy_vfs::TabSource;
use hxy_vfs::VfsRegistry;
use hxy_vfs::handlers::ZipHandler;
use hxy_view::HexView;

use crate::APP_NAME;
use crate::file::FileId;
use crate::file::OpenFile;
use crate::state::PersistedState;
use crate::state::SharedPersistedState;
use crate::tabs::Tab;
use crate::window::WindowSettings;

pub struct HxyApp {
    dock: DockState<Tab>,
    files: HashMap<FileId, OpenFile>,
    state: SharedPersistedState,
    next_file_id: u64,
    registry: VfsRegistry,
    commands: Vec<Box<dyn crate::commands::ToolbarCommand>>,

    #[cfg(not(target_arch = "wasm32"))]
    sink: Option<crate::persist::SaveSink>,

    /// Window geometry captured last frame, used to detect drag-end: the
    /// first frame where `prev_window == current_window` and the saved
    /// value still differs triggers the persistence write.
    prev_window: Option<WindowSettings>,
    last_saved_window: Option<WindowSettings>,

    /// Zoom factor we last applied to the egui context. Used to push
    /// settings changes into the live context without re-running every
    /// frame.
    applied_zoom: f32,
}

impl HxyApp {
    pub fn new(cc: &eframe::CreationContext<'_>, state: SharedPersistedState) -> Self {
        install_fonts(&cc.egui_ctx);
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        let (initial_zoom, initial_window) = {
            let s = state.read();
            (s.app.zoom_factor, s.window)
        };
        cc.egui_ctx.set_zoom_factor(initial_zoom);
        let mut registry = VfsRegistry::new();
        registry.register(Arc::new(ZipHandler::new()));
        Self {
            dock: DockState::new(vec![Tab::Welcome, Tab::Settings]),
            files: HashMap::new(),
            state,
            next_file_id: 1,
            registry,
            commands: crate::commands::default_commands(),
            #[cfg(not(target_arch = "wasm32"))]
            sink: None,
            prev_window: None,
            last_saved_window: Some(initial_window),
            applied_zoom: initial_zoom,
        }
    }

    pub fn registry(&self) -> &VfsRegistry {
        &self.registry
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn with_sink(mut self, sink: crate::persist::SaveSink) -> Self {
        self.sink = Some(sink);
        self.restore_open_tabs();
        self
    }

    fn fresh_file_id(&mut self) -> FileId {
        let id = FileId::new(self.next_file_id);
        self.next_file_id += 1;
        id
    }

    pub fn open_in_memory(&mut self, display_name: impl Into<String>, bytes: Vec<u8>) -> FileId {
        self.open(display_name, None, bytes, None, None)
    }

    pub fn open_filesystem(
        &mut self,
        display_name: impl Into<String>,
        path: std::path::PathBuf,
        bytes: Vec<u8>,
        restore_selection: Option<hxy_core::Selection>,
        restore_scroll: Option<f32>,
    ) -> FileId {
        self.open(display_name, Some(TabSource::Filesystem(path)), bytes, restore_selection, restore_scroll)
    }

    /// Open a new file tab with the given display name, persistent
    /// source identity, and byte contents. Runs format detection
    /// against the source's first bytes and caches the matching handler
    /// (if any) on the tab so the toolbar command can enable itself.
    pub fn open(
        &mut self,
        display_name: impl Into<String>,
        source_kind: Option<TabSource>,
        bytes: Vec<u8>,
        restore_selection: Option<hxy_core::Selection>,
        restore_scroll: Option<f32>,
    ) -> FileId {
        let id = self.fresh_file_id();
        let mut file = OpenFile::from_bytes(id, display_name, source_kind.clone(), bytes);
        file.selection = restore_selection;
        if let Some(s) = restore_scroll {
            file.pending_scroll = Some(s);
            file.scroll_offset = s;
        }

        // Detect a matching VFS handler against the first ~4 KiB.
        if let Ok(range) = hxy_core::ByteRange::new(
            hxy_core::ByteOffset::new(0),
            hxy_core::ByteOffset::new(file.source.len().get().min(4096)),
        ) && let Ok(head) = file.source.read(range)
        {
            file.detected_handler = self.registry.detect(&head);
        }

        self.files.insert(id, file);
        self.dock.push_to_focused_leaf(Tab::File(id));

        if let Some(source) = source_kind {
            let mut g = self.state.write();
            if let TabSource::Filesystem(p) = &source {
                g.app.record_recent(p.clone());
            }
            if !g.open_tabs.iter().any(|t| t.source == source) {
                g.open_tabs.push(crate::state::OpenTabState {
                    source,
                    selection: restore_selection,
                    scroll_offset: restore_scroll.unwrap_or(0.0),
                });
            }
        }
        id
    }

    /// Try to open each saved tab. Filesystem tabs are read directly
    /// from disk; VFS-entry tabs require their parent tab to be open
    /// with a materialised mount. We sort tabs by `TabSource` depth so
    /// parents are restored before their children. Failures (file
    /// missing, parent failed to mount, entry path gone) drop the tab
    /// from the persisted list.
    #[cfg(not(target_arch = "wasm32"))]
    fn restore_open_tabs(&mut self) {
        let mut tabs = self.state.read().open_tabs.clone();
        // Topologically order: shallower depth first so parents load
        // before any child that references them.
        tabs.sort_by_key(|t| t.source.depth());

        let mut surviving: Vec<crate::state::OpenTabState> = Vec::new();
        for tab in tabs {
            let result = self.restore_one_tab(&tab);
            match result {
                Ok(()) => surviving.push(tab),
                Err(e) => {
                    tracing::warn!(error = %e, "restore open tab");
                }
            }
        }
        self.state.write().open_tabs = surviving;
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn restore_one_tab(&mut self, tab: &crate::state::OpenTabState) -> Result<(), crate::file::FileOpenError> {
        match &tab.source {
            TabSource::Filesystem(path) => {
                let bytes = std::fs::read(path)
                    .map_err(|source| crate::file::FileOpenError::Read { path: path.clone(), source })?;
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                self.open(name, Some(tab.source.clone()), bytes, tab.selection, Some(tab.scroll_offset));
                Ok(())
            }
            TabSource::VfsEntry { parent, entry_path } => {
                // Parent must already exist as an open tab with a mount.
                let parent_file_id = self
                    .files
                    .iter()
                    .find_map(|(id, f)| (f.source_kind.as_ref() == Some(parent.as_ref())).then_some(*id))
                    .ok_or_else(|| parent_missing(parent.as_ref()))?;
                let parent_mount = self
                    .files
                    .get(&parent_file_id)
                    .and_then(|f| f.mount.clone())
                    .ok_or_else(|| parent_missing(parent.as_ref()))?;
                let bytes = read_vfs_entry(&*parent_mount.fs, entry_path)
                    .map_err(|e| crate::file::FileOpenError::Read { path: entry_path.into(), source: e })?;
                let name = entry_path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(entry_path).to_owned();
                self.open(name, Some(tab.source.clone()), bytes, tab.selection, Some(tab.scroll_offset));
                Ok(())
            }
        }
    }

    /// Save the current state if it has drifted from what was last written.
    /// No-op on wasm (no sink yet).
    fn save_if_dirty(&mut self, snapshot_before: &PersistedState) {
        let after = self.state.read().clone();
        if *snapshot_before == after {
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(sink) = &self.sink {
            if let Err(e) = sink.save(&after) {
                tracing::warn!(error = %e, "save persisted state");
            } else {
                self.last_saved_window = Some(after.window);
            }
        }
    }
}

impl eframe::App for HxyApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let snapshot_before = self.state.read().clone();

        top_menu_bar(ui, self);
        render_toolbar_and_apply(ui, self);

        {
            let mut state_guard = self.state.write();
            let mut viewer = HxyTabViewer { files: &mut self.files, state: &mut state_guard };
            let style = Style::from_egui(ui.style());
            DockArea::new(&mut self.dock).style(style).show_inside(ui, &mut viewer);
        }

        apply_zoom_change(ui.ctx(), &self.state, &mut self.applied_zoom);

        capture_window_on_drag_end(ui.ctx(), &self.state, &mut self.prev_window, &self.last_saved_window);

        paint_drop_overlay(ui.ctx());
        consume_dropped_files(ui.ctx(), self);
        consume_welcome_open_request(ui.ctx(), self);
        dispatch_copy_shortcut(ui.ctx(), self);

        self.save_if_dirty(&snapshot_before);
    }
}

/// App-level copy shortcut handler. Runs after the dock renders, so
/// per-widget hover-copy (status bar labels) has already had a chance
/// to consume the event. Whatever's left dispatches to the currently
/// active file tab.
fn dispatch_copy_shortcut(ctx: &egui::Context, app: &mut HxyApp) {
    let kind = ctx.input_mut(|i| {
        if i.consume_shortcut(&COPY_HEX) {
            Some(CopyKind::Hex)
        } else if consume_copy_event(i) {
            Some(CopyKind::Bytes)
        } else {
            None
        }
    });
    let Some(kind) = kind else { return };
    let file_id =
        app.dock.find_active_focused().and_then(|(_, tab)| if let Tab::File(id) = *tab { Some(id) } else { None });
    if let Some(id) = file_id
        && let Some(file) = app.files.get(&id)
    {
        do_copy(ctx, file, kind);
    }
}

/// Consume the plain "copy" shortcut in all the forms the integration
/// might deliver it: as an `Event::Copy` (winit on macOS converts Cmd+C
/// to a semantic copy event), or as a normal `Event::Key` with the
/// Command modifier on platforms that pass it through.
fn consume_copy_event(input: &mut egui::InputState) -> bool {
    let had_copy = input.events.iter().any(|e| matches!(e, egui::Event::Copy));
    if had_copy {
        input.events.retain(|e| !matches!(e, egui::Event::Copy));
        return true;
    }
    input.consume_shortcut(&COPY_BYTES)
}

fn consume_welcome_open_request(ctx: &egui::Context, app: &mut HxyApp) {
    let req = ctx.data_mut(|d| d.remove_temp::<std::path::PathBuf>(egui::Id::new(WELCOME_OPEN_RECENT)));
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(path) = req {
        match std::fs::read(&path) {
            Ok(bytes) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                app.open_filesystem(name, path, bytes, None, None);
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "open recent file");
            }
        }
    }
    #[cfg(target_arch = "wasm32")]
    let _ = (req, app);
}

fn paint_drop_overlay(ctx: &egui::Context) {
    let hovered_count = ctx.input(|i| i.raw.hovered_files.len());
    if hovered_count == 0 {
        return;
    }
    let text = ctx.input(|i| {
        if i.raw.hovered_files.len() > 1 {
            return "Drop one file at a time".to_owned();
        }
        let Some(file) = i.raw.hovered_files.first() else {
            return "Drop a file".to_owned();
        };
        match file.path.as_deref().and_then(|p| p.file_name()).and_then(|n| n.to_str()) {
            Some(name) => format!("Drop to open\n{name}"),
            None => "Drop to open".to_owned(),
        }
    });
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("hxy_drop_target")));
    let screen = ctx.content_rect();
    painter.rect_filled(screen, 0.0, egui::Color32::from_black_alpha(192));
    painter.text(
        screen.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::TextStyle::Heading.resolve(&ctx.global_style()),
        egui::Color32::WHITE,
    );
}

fn consume_dropped_files(ctx: &egui::Context, app: &mut HxyApp) {
    let dropped = ctx.input(|i| i.raw.dropped_files.clone());
    for file in dropped {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(path) = file.path {
            match std::fs::read(&path) {
                Ok(bytes) => {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    app.open_filesystem(name, path, bytes, None, None);
                }
                Err(e) => {
                    tracing::warn!(error = %e, path = %path.display(), "open dropped file");
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        if let Some(bytes) = file.bytes.as_deref() {
            if !bytes.is_empty() {
                let name = if file.name.is_empty() { "dropped".to_string() } else { file.name.clone() };
                app.open_in_memory(name, bytes.to_vec());
            }
        }
    }
}

/// Push the current `settings.zoom_factor` into the egui context whenever
/// it drifts from what we last applied.
fn apply_zoom_change(ctx: &egui::Context, state: &SharedPersistedState, applied: &mut f32) {
    let target = state.read().app.zoom_factor;
    if (target - *applied).abs() > f32::EPSILON {
        ctx.set_zoom_factor(target);
        *applied = target;
    }
}

/// Read the current viewport's window geometry; push it into the shared
/// state only when geometry has been stable for at least one frame and
/// differs from the last persisted value. This is the drag-end signal.
fn capture_window_on_drag_end(
    ctx: &egui::Context,
    state: &SharedPersistedState,
    prev_window: &mut Option<WindowSettings>,
    last_saved_window: &Option<WindowSettings>,
) {
    let zoom = state.read().app.zoom_factor;
    let current = ctx
        .input(|i| i.raw.viewports.get(&i.raw.viewport_id).map(|info| WindowSettings::from_viewport_info(info, zoom)));
    let Some(current) = current else {
        return;
    };
    let stable = prev_window.as_ref() == Some(&current);
    *prev_window = Some(current);
    if !stable {
        return;
    }
    if last_saved_window.as_ref() == Some(&current) {
        return;
    }
    let mut g = state.write();
    if g.window != current {
        g.window = current;
    }
}

fn render_toolbar_and_apply(ui: &mut egui::Ui, app: &mut HxyApp) {
    use crate::commands::CommandEffect;
    use crate::commands::ToolbarCtx;

    let active_file_id = active_file_id(app);
    // Resolve styles + icon font once outside the borrow so the command
    // trait objects can read them via the context below.
    let mut effects: Vec<CommandEffect> = Vec::new();

    // Snapshot the command list off `app` so we can borrow other fields
    // of `app` through `ToolbarCtx`. Commands are `Send + Sync` and are
    // owned trait objects — moving them out and back is cheap (they're
    // zero-size types in practice).
    let commands = std::mem::take(&mut app.commands);

    egui::Panel::top("hxy_toolbar")
        .resizable(false)
        .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(6, 4)))
        .show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                let mut state_guard = app.state.write();
                let active_file = active_file_id.and_then(|id| app.files.get_mut(&id));
                let ctx_handle = ui.ctx().clone();
                let mut cx = ToolbarCtx {
                    ctx: &ctx_handle,
                    state: &mut state_guard,
                    active_file,
                    active_file_id,
                    effects: &mut effects,
                };
                for cmd in &commands {
                    let enabled = cmd.enabled(&cx);
                    let label = cmd.label(&cx);
                    let icon = cmd.icon();
                    let btn = egui::Button::new(egui::RichText::new(icon).size(16.0)).frame(false);
                    let r = ui.add_enabled(enabled, btn).on_hover_text(&label);
                    if r.clicked() {
                        cmd.invoke(&mut cx);
                    }
                }
            });
        });

    app.commands = commands;

    for effect in effects {
        apply_command_effect(ui.ctx(), app, effect);
    }
}

fn apply_command_effect(_ctx: &egui::Context, app: &mut HxyApp, effect: crate::commands::CommandEffect) {
    use crate::commands::CommandEffect;
    match effect {
        CommandEffect::OpenFileDialog => handle_open_file(app),
        CommandEffect::MountActiveFile => mount_active_file(app),
        CommandEffect::OpenRecent(path) => {
            #[cfg(not(target_arch = "wasm32"))]
            match std::fs::read(&path) {
                Ok(bytes) => {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    app.open_filesystem(name, path, bytes, None, None);
                }
                Err(e) => tracing::warn!(error = %e, path = %path.display(), "open recent"),
            }
            #[cfg(target_arch = "wasm32")]
            let _ = path;
        }
    }
}

/// Invoke the active tab's detected handler to mount its source into a
/// browsable VFS. On success, the tree panel picks it up automatically
/// because it reads `file.mount`.
fn mount_active_file(app: &mut HxyApp) {
    let Some(id) = active_file_id(app) else { return };
    let Some(file) = app.files.get_mut(&id) else { return };
    if file.mount.is_some() {
        return;
    }
    let Some(handler) = file.detected_handler.clone() else { return };
    match handler.mount(file.source.clone()) {
        Ok(mount) => file.mount = Some(Arc::new(mount)),
        Err(e) => tracing::warn!(error = %e, handler = handler.name(), "mount vfs"),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn parent_missing(parent: &TabSource) -> crate::file::FileOpenError {
    crate::file::FileOpenError::Read {
        path: std::path::PathBuf::from(format!("{parent:?}")),
        source: std::io::Error::new(std::io::ErrorKind::NotFound, "parent tab / mount not available"),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn read_vfs_entry(fs: &dyn hxy_vfs::vfs::FileSystem, path: &str) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut file = fs.open_file(path).map_err(|e| std::io::Error::other(format!("open {path}: {e}")))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);
}

fn top_menu_bar(ui: &mut egui::Ui, app: &mut HxyApp) {
    egui::Panel::top("hxy_menu_bar").show_inside(ui, |ui| {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button(hxy_i18n::t("menu-file"), |ui| {
                if ui.button(hxy_i18n::t("menu-file-open")).clicked() {
                    ui.close();
                    handle_open_file(app);
                }
                ui.separator();
                if ui.button(hxy_i18n::t("menu-file-quit")).clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button(hxy_i18n::t("menu-edit"), |ui| {
                let copy_bytes_text = ui.ctx().format_shortcut(&COPY_BYTES);
                let copy_hex_text = ui.ctx().format_shortcut(&COPY_HEX);
                let active_file = active_file_id(app);
                ui.add_enabled_ui(active_file.is_some(), |ui| {
                    if ui
                        .add(egui::Button::new(hxy_i18n::t("menu-edit-copy-bytes")).shortcut_text(copy_bytes_text))
                        .clicked()
                    {
                        if let Some(id) = active_file
                            && let Some(file) = app.files.get(&id)
                        {
                            do_copy(ui.ctx(), file, CopyKind::Bytes);
                        }
                        ui.close();
                    }
                    if ui
                        .add(egui::Button::new(hxy_i18n::t("menu-edit-copy-hex")).shortcut_text(copy_hex_text))
                        .clicked()
                    {
                        if let Some(id) = active_file
                            && let Some(file) = app.files.get(&id)
                        {
                            do_copy(ui.ctx(), file, CopyKind::Hex);
                        }
                        ui.close();
                    }
                });
            });
            ui.menu_button(hxy_i18n::t("menu-help"), |ui| {
                ui.label(format!("{APP_NAME} {}", env!("CARGO_PKG_VERSION")));
            });
        });
    });
}

fn active_file_id(app: &mut HxyApp) -> Option<FileId> {
    app.dock.find_active_focused().and_then(|(_, tab)| if let Tab::File(id) = *tab { Some(id) } else { None })
}

fn handle_open_file(app: &mut HxyApp) {
    #[cfg(not(target_arch = "wasm32"))]
    match pick_and_read_file() {
        Ok((name, path, bytes)) => {
            app.open_filesystem(name, path, bytes, None, None);
        }
        Err(crate::file::FileOpenError::Cancelled) => {}
        Err(e) => {
            tracing::warn!(error = %e, "open file");
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = app;
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn pick_and_read_file() -> Result<(String, std::path::PathBuf, Vec<u8>), crate::file::FileOpenError> {
    let Some(path) = rfd::FileDialog::new().pick_file() else {
        return Err(crate::file::FileOpenError::Cancelled);
    };
    let bytes =
        std::fs::read(&path).map_err(|source| crate::file::FileOpenError::Read { path: path.clone(), source })?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
    Ok((name, path, bytes))
}

struct HxyTabViewer<'a> {
    files: &'a mut HashMap<FileId, OpenFile>,
    state: &'a mut PersistedState,
}

impl TabViewer for HxyTabViewer<'_> {
    type Tab = Tab;

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        match tab {
            Tab::Welcome => hxy_i18n::t("tab-welcome").into(),
            Tab::Settings => hxy_i18n::t("tab-settings").into(),
            Tab::File(id) => match self.files.get(id) {
                Some(f) => f.display_name.clone().into(),
                None => format!("file-{}", id.get()).into(),
            },
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        match tab {
            Tab::Welcome => welcome_ui(ui, self.state),
            Tab::Settings => settings_ui(ui, &mut self.state.app),
            Tab::File(id) => match self.files.get_mut(id) {
                Some(file) => {
                    let settings_base = self.state.app.offset_base;
                    let mut new_base = settings_base;
                    let highlight =
                        self.state.app.byte_value_highlight.then(|| self.state.app.byte_highlight_mode.as_view());
                    let palette = build_palette(ui.visuals().dark_mode, &self.state.app, highlight);
                    let mut copy_request: Option<CopyKind> = None;
                    let has_sel = file.selection.map(|s| !s.range().is_empty()).unwrap_or(false);
                    let pending_scroll = file.pending_scroll.take();

                    // Explicit layout: split the tab into hex (top) and
                    // status bar (bottom). Both share the same painted
                    // background. The status bar rect is sized to fit
                    // the text + symmetric padding with no leftover
                    // vertical space, and uses a centered layout so the
                    // text sits in the middle of its rect.
                    let tab_rect = ui.available_rect_before_wrap();
                    let bg = ui.visuals().window_fill();
                    ui.painter().rect_filled(tab_rect, 0.0, bg);

                    let text_h = ui.text_style_height(&egui::TextStyle::Body);
                    let status_h = text_h + 2.0;
                    let status_top_y = tab_rect.bottom() - status_h;
                    let hex_rect =
                        egui::Rect::from_min_max(tab_rect.min, egui::Pos2::new(tab_rect.right(), status_top_y));
                    let status_rect =
                        egui::Rect::from_min_max(egui::Pos2::new(tab_rect.left(), status_top_y), tab_rect.max);

                    ui.painter().hline(
                        tab_rect.x_range(),
                        status_top_y,
                        egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
                    );

                    let mut view = HexView::new(&*file.source, &mut file.selection)
                        .columns(self.state.app.hex_columns)
                        .value_highlight(highlight)
                        .minimap(self.state.app.show_minimap)
                        .minimap_colored(self.state.app.minimap_colored);
                    if let Some(p) = palette {
                        view = view.palette(p);
                    }
                    if let Some(s) = pending_scroll {
                        view = view.scroll_to(s);
                    }
                    let response = ui
                        .scope_builder(egui::UiBuilder::new().max_rect(hex_rect), |ui| {
                            view.context_menu(|ui| {
                                ui.add_enabled_ui(has_sel, |ui| {
                                    if ui.button("Copy bytes").clicked() {
                                        copy_request = Some(CopyKind::Bytes);
                                        ui.close();
                                    }
                                    if ui.button("Copy hex string").clicked() {
                                        copy_request = Some(CopyKind::Hex);
                                        ui.close();
                                    }
                                });
                            })
                            .show(ui)
                        })
                        .inner;
                    file.hovered = response.hovered_offset;
                    file.scroll_offset = response.scroll_offset;
                    sync_tab_state(self.state, file);

                    // Re-assert the background fill over the status bar
                    // rect right before painting its content, so whatever
                    // the hex view may or may not have drawn below its
                    // rect doesn't show through.
                    ui.painter().rect_filled(status_rect, 0.0, bg);
                    ui.scope_builder(
                        egui::UiBuilder::new()
                            .max_rect(status_rect.shrink2(egui::Vec2::new(8.0, 0.0)))
                            .layout(egui::Layout::left_to_right(egui::Align::Center)),
                        |ui| {
                            status_bar_ui(ui, file, settings_base, &mut new_base);
                        },
                    );

                    if let Some(kind) = copy_request {
                        do_copy(ui.ctx(), file, kind);
                    }
                    if new_base != settings_base {
                        self.state.app.offset_base = new_base;
                    }
                }
                None => {
                    ui.colored_label(egui::Color32::RED, format!("missing file {id:?}"));
                }
            },
        }
    }

    fn closeable(&mut self, tab: &mut Self::Tab) -> bool {
        matches!(tab, Tab::File(_))
    }

    fn scroll_bars(&self, tab: &Self::Tab) -> [bool; 2] {
        // File tabs manage their own scrolling via the hex view's
        // internal scroll area + minimap — no outer dock scrollbar.
        if matches!(tab, Tab::File(_)) { [false, false] } else { [true, true] }
    }

    fn on_close(&mut self, tab: &mut Self::Tab) -> OnCloseResponse {
        if let Tab::File(id) = tab
            && let Some(removed) = self.files.remove(id)
            && let Some(source) = removed.source_kind
        {
            self.state.open_tabs.retain(|t| t.source != source);
        }
        OnCloseResponse::Close
    }
}

/// Mirror the tab's in-memory selection + scroll into
/// [`PersistedState::open_tabs`] so the save-on-dirty path picks it up.
fn sync_tab_state(state: &mut PersistedState, file: &OpenFile) {
    let Some(source) = &file.source_kind else { return };
    if let Some(entry) = state.open_tabs.iter_mut().find(|t| &t.source == source) {
        entry.selection = file.selection;
        entry.scroll_offset = file.scroll_offset;
    }
}

fn build_palette(
    dark: bool,
    settings: &crate::settings::AppSettings,
    highlight: Option<hxy_view::ValueHighlight>,
) -> Option<hxy_view::HighlightPalette> {
    let mode = highlight?;
    Some(match settings.byte_highlight_scheme {
        crate::settings::ByteHighlightScheme::Class => {
            hxy_view::HighlightPalette::Class(hxy_view::BytePalette::for_theme_and_mode(dark, mode))
        }
        crate::settings::ByteHighlightScheme::Value => {
            hxy_view::HighlightPalette::Value(hxy_view::ValueGradient::for_theme_and_mode(dark, mode))
        }
    })
}

fn status_bar_ui(
    ui: &mut egui::Ui,
    file: &OpenFile,
    base: crate::settings::OffsetBase,
    new_base: &mut crate::settings::OffsetBase,
) {
    ui.horizontal(|ui| {
        if let Some(hov) = file.hovered {
            let value = format_offset(hov.get(), base);
            copyable_status_label(
                ui,
                &format!("Hover: {value}"),
                &value,
                Some(format_offset(hov.get(), base.toggle())),
                new_base,
                base,
            );
        } else {
            ui.label("Hover: —");
        }
        ui.separator();
        if let Some(sel) = file.selection {
            let range = sel.range();
            let last_inclusive = range.end().get().saturating_sub(1);
            let (display, copy, tooltip) = if sel.is_caret() {
                let v = format_offset(range.start().get(), base);
                (format!("Caret: {v}"), v, format_offset(range.start().get(), base.toggle()))
            } else {
                let start = format_offset(range.start().get(), base);
                let end = format_offset(last_inclusive, base);
                let len = range.len().get();
                let copy_value = format!("{start}–{end} ({len} bytes)");
                let tooltip = format!(
                    "{}–{}",
                    format_offset(range.start().get(), base.toggle()),
                    format_offset(last_inclusive, base.toggle()),
                );
                (format!("Sel: {copy_value}"), copy_value, tooltip)
            };
            copyable_status_label(ui, &display, &copy, Some(tooltip), new_base, base);
        } else {
            ui.label("Sel: —");
        }

        let size = file.source.len().get();
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let value = format_offset(size, base);
            copyable_status_label(
                ui,
                &format!("Length: {value}"),
                &value,
                Some(format_offset(size, base.toggle())),
                new_base,
                base,
            );
        });
    });
}

/// Click to toggle offset base, hover for the alternate-base tooltip,
/// and — while hovered — consume Cmd/Ctrl+C to copy the label's text.
/// Consuming the shortcut keeps the hex-view selection copy handler
/// from also firing in the same frame.
fn copyable_status_label(
    ui: &mut egui::Ui,
    display: &str,
    copy: &str,
    tooltip: Option<String>,
    new_base: &mut crate::settings::OffsetBase,
    base: crate::settings::OffsetBase,
) {
    let r = ui.add(egui::Label::new(display).sense(egui::Sense::click()));
    if r.clicked() {
        *new_base = base.toggle();
    }
    let r = if let Some(tt) = tooltip { r.on_hover_text(tt) } else { r };
    if r.hovered() && ui.ctx().input_mut(consume_copy_event) {
        ui.ctx().copy_text(copy.to_string());
    }
}

fn format_offset(value: u64, base: crate::settings::OffsetBase) -> String {
    match base {
        crate::settings::OffsetBase::Hex => format!("0x{value:X}"),
        crate::settings::OffsetBase::Decimal => format!("{value}"),
    }
}

#[derive(Clone, Copy)]
enum CopyKind {
    Bytes,
    Hex,
}

const COPY_BYTES: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::C);
const COPY_HEX: egui::KeyboardShortcut =
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT), egui::Key::C);

fn do_copy(ctx: &egui::Context, file: &OpenFile, kind: CopyKind) {
    let Some(selection) = file.selection else { return };
    let range = selection.range();
    if range.is_empty() {
        return;
    }
    let bytes = match file.source.read(range) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "read selection for copy");
            return;
        }
    };
    let text = match kind {
        CopyKind::Bytes => String::from_utf8_lossy(&bytes).into_owned(),
        CopyKind::Hex => format_hex_string(&bytes),
    };
    ctx.copy_text(text);
}

fn format_hex_string(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        use std::fmt::Write;
        let _ = write!(out, "{b:02X}");
    }
    out
}

const WELCOME_OPEN_RECENT: &str = "hxy_welcome_open_recent";

fn welcome_ui(ui: &mut egui::Ui, state: &PersistedState) {
    ui.vertical_centered(|ui| {
        ui.add_space(32.0);
        ui.heading(hxy_i18n::t("app-name"));
        ui.label(hxy_i18n::t("app-tagline"));
    });
    ui.add_space(16.0);
    ui.separator();
    ui.add_space(8.0);
    ui.heading(hxy_i18n::t("welcome-recent"));
    if state.app.recent_files.is_empty() {
        ui.weak(hxy_i18n::t("welcome-recent-empty"));
        return;
    }
    egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
        for entry in &state.app.recent_files {
            let label = entry.path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let row = ui
                .add(egui::Button::new(label).wrap_mode(egui::TextWrapMode::Truncate))
                .on_hover_text(entry.path.display().to_string());
            if row.clicked() {
                ui.ctx().data_mut(|d| d.insert_temp(egui::Id::new(WELCOME_OPEN_RECENT), entry.path.clone()));
            }
        }
    });
}

fn settings_ui(ui: &mut egui::Ui, settings: &mut crate::settings::AppSettings) {
    ui.heading(hxy_i18n::t("settings-general-header"));
    ui.separator();
    egui::Grid::new("hxy-general-settings").num_columns(2).striped(true).show(ui, |ui| {
        ui.label(hxy_i18n::t("settings-zoom"));
        ui.add(egui::Slider::new(&mut settings.zoom_factor, 0.5..=2.0).step_by(0.1));
        ui.end_row();

        ui.label(hxy_i18n::t("settings-columns"));
        let mut cols = settings.hex_columns.get();
        ui.add(egui::DragValue::new(&mut cols).range(1..=64));
        if let Ok(new_cols) = hxy_core::ColumnCount::new(cols) {
            settings.hex_columns = new_cols;
        }
        ui.end_row();

        ui.label(hxy_i18n::t("settings-check-updates"));
        ui.checkbox(&mut settings.check_for_updates, "");
        ui.end_row();

        ui.label(hxy_i18n::t("settings-byte-highlight"));
        ui.checkbox(&mut settings.byte_value_highlight, "");
        ui.end_row();

        ui.label(hxy_i18n::t("settings-byte-highlight-mode"));
        ui.horizontal(|ui| {
            ui.selectable_value(
                &mut settings.byte_highlight_mode,
                crate::settings::ByteHighlightMode::Background,
                hxy_i18n::t("settings-byte-highlight-background"),
            );
            ui.selectable_value(
                &mut settings.byte_highlight_mode,
                crate::settings::ByteHighlightMode::Text,
                hxy_i18n::t("settings-byte-highlight-text"),
            );
        });
        ui.end_row();

        ui.label(hxy_i18n::t("settings-byte-highlight-scheme"));
        ui.horizontal(|ui| {
            ui.selectable_value(
                &mut settings.byte_highlight_scheme,
                crate::settings::ByteHighlightScheme::Class,
                hxy_i18n::t("settings-byte-highlight-scheme-class"),
            );
            ui.selectable_value(
                &mut settings.byte_highlight_scheme,
                crate::settings::ByteHighlightScheme::Value,
                hxy_i18n::t("settings-byte-highlight-scheme-value"),
            );
        });
        ui.end_row();

        ui.label(hxy_i18n::t("settings-minimap"));
        ui.checkbox(&mut settings.show_minimap, "");
        ui.end_row();

        ui.label(hxy_i18n::t("settings-minimap-colored"));
        ui.add_enabled_ui(settings.show_minimap, |ui| {
            ui.checkbox(&mut settings.minimap_colored, "");
        });
        ui.end_row();

        ui.label(hxy_i18n::t("settings-offset-base"));
        egui::ComboBox::from_id_salt("hxy-offset-base")
            .selected_text(match settings.offset_base {
                crate::settings::OffsetBase::Hex => "Hex",
                crate::settings::OffsetBase::Decimal => "Decimal",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut settings.offset_base, crate::settings::OffsetBase::Hex, "Hex");
                ui.selectable_value(&mut settings.offset_base, crate::settings::OffsetBase::Decimal, "Decimal");
            });
        ui.end_row();
    });
}
