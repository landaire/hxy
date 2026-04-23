//! Main application type.

use std::collections::HashMap;

use egui_dock::DockArea;
use egui_dock::DockState;
use egui_dock::Style;
use egui_dock::TabViewer;
use egui_dock::tab_viewer::OnCloseResponse;
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
        Self {
            dock: DockState::new(vec![Tab::Welcome, Tab::Settings]),
            files: HashMap::new(),
            state,
            next_file_id: 1,
            #[cfg(not(target_arch = "wasm32"))]
            sink: None,
            prev_window: None,
            last_saved_window: Some(initial_window),
            applied_zoom: initial_zoom,
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn with_sink(mut self, sink: crate::persist::SaveSink) -> Self {
        self.sink = Some(sink);
        self
    }

    fn fresh_file_id(&mut self) -> FileId {
        let id = FileId::new(self.next_file_id);
        self.next_file_id += 1;
        id
    }

    pub fn open_in_memory(&mut self, display_name: impl Into<String>, bytes: Vec<u8>) -> FileId {
        self.open_in_memory_with_path(display_name, None, bytes)
    }

    pub fn open_in_memory_with_path(
        &mut self,
        display_name: impl Into<String>,
        path: Option<std::path::PathBuf>,
        bytes: Vec<u8>,
    ) -> FileId {
        let id = self.fresh_file_id();
        let file = OpenFile::from_bytes(id, display_name, path, bytes);
        self.files.insert(id, file);
        self.dock.push_to_focused_leaf(Tab::File(id));
        id
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

        {
            let mut state_guard = self.state.write();
            let mut viewer = HxyTabViewer { files: &mut self.files, state: &mut state_guard };
            let style = Style::from_egui(ui.style());
            DockArea::new(&mut self.dock).style(style).show_inside(ui, &mut viewer);
        }

        apply_zoom_change(ui.ctx(), &self.state, &mut self.applied_zoom);

        capture_window_on_drag_end(ui.ctx(), &self.state, &mut self.prev_window, &self.last_saved_window);

        self.save_if_dirty(&snapshot_before);
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
            ui.menu_button(hxy_i18n::t("menu-help"), |ui| {
                ui.label(format!("{APP_NAME} {}", env!("CARGO_PKG_VERSION")));
            });
        });
    });
}

fn handle_open_file(app: &mut HxyApp) {
    #[cfg(not(target_arch = "wasm32"))]
    match pick_and_read_file() {
        Ok((name, path, bytes)) => {
            app.open_in_memory_with_path(name, Some(path), bytes);
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
            Tab::Welcome => welcome_ui(ui),
            Tab::Settings => settings_ui(ui, &mut self.state.app),
            Tab::File(id) => match self.files.get(id) {
                Some(file) => {
                    let view = HexView::new(&*file.source).columns(self.state.app.hex_columns);
                    let view = match file.selection.as_ref() {
                        Some(sel) => view.selection(sel),
                        None => view,
                    };
                    view.show(ui);
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

    fn on_close(&mut self, tab: &mut Self::Tab) -> OnCloseResponse {
        if let Tab::File(id) = tab {
            self.files.remove(id);
        }
        OnCloseResponse::Close
    }
}

fn welcome_ui(ui: &mut egui::Ui) {
    ui.vertical_centered(|ui| {
        ui.add_space(32.0);
        ui.heading(hxy_i18n::t("app-name"));
        ui.label(hxy_i18n::t("app-tagline"));
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
    });
}
