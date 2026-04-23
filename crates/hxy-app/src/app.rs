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

pub struct HxyApp {
    dock: DockState<Tab>,
    files: HashMap<FileId, OpenFile>,
    state: SharedPersistedState,
    next_file_id: u64,
    #[cfg(not(target_arch = "wasm32"))]
    save_notify: Option<std::sync::Arc<tokio::sync::Notify>>,
    last_saved_zoom: f32,
}

impl HxyApp {
    pub fn new(cc: &eframe::CreationContext<'_>, state: SharedPersistedState) -> Self {
        install_fonts(&cc.egui_ctx);
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        let initial_zoom = state.read().app.zoom_factor;
        cc.egui_ctx.set_zoom_factor(initial_zoom);
        Self {
            dock: DockState::new(vec![Tab::Welcome, Tab::Settings]),
            files: HashMap::new(),
            state,
            next_file_id: 1,
            #[cfg(not(target_arch = "wasm32"))]
            save_notify: None,
            last_saved_zoom: initial_zoom,
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn with_save_notify(mut self, notify: std::sync::Arc<tokio::sync::Notify>) -> Self {
        self.save_notify = Some(notify);
        self
    }

    fn fresh_file_id(&mut self) -> FileId {
        let id = FileId::new(self.next_file_id);
        self.next_file_id += 1;
        id
    }

    pub fn open_in_memory(&mut self, display_name: impl Into<String>, bytes: Vec<u8>) -> FileId {
        let id = self.fresh_file_id();
        let file = OpenFile::from_bytes(id, display_name, None, bytes);
        self.files.insert(id, file);
        self.dock.push_to_focused_leaf(Tab::File(id));
        id
    }

    fn notify_save(&self) {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(notify) = &self.save_notify {
            notify.notify_one();
        }
    }
}

impl eframe::App for HxyApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let before = serialize_settings_for_diff(&self.state.read());

        top_menu_bar(ui, self);

        let mut state_guard = self.state.write();
        let mut viewer = HxyTabViewer { files: &mut self.files, state: &mut state_guard };
        let style = Style::from_egui(ui.style());
        DockArea::new(&mut self.dock).style(style).show_inside(ui, &mut viewer);
        drop(state_guard);

        let app_settings_now = self.state.read().app.clone();
        if (app_settings_now.zoom_factor - self.last_saved_zoom).abs() > f32::EPSILON {
            ui.ctx().set_zoom_factor(app_settings_now.zoom_factor);
            self.last_saved_zoom = app_settings_now.zoom_factor;
        }

        capture_window_info(ui.ctx(), &self.state, app_settings_now.zoom_factor);

        let after = serialize_settings_for_diff(&self.state.read());
        if before != after {
            self.notify_save();
        }
    }
}

fn serialize_settings_for_diff(state: &PersistedState) -> Option<String> {
    serde_json::to_string(&(&state.app, &state.window)).ok()
}

fn capture_window_info(ctx: &egui::Context, state: &SharedPersistedState, zoom_factor: f32) {
    ctx.input(|i| {
        let Some(info) = i.raw.viewports.get(&i.raw.viewport_id) else {
            return;
        };
        let new = crate::window::WindowSettings::from_viewport_info(info, zoom_factor);
        let mut g = state.write();
        if !window_matches(&g.window, &new) {
            g.window = new;
        }
    });
}

fn window_matches(a: &crate::window::WindowSettings, b: &crate::window::WindowSettings) -> bool {
    a.inner_size_points == b.inner_size_points
        && a.outer_position_pixels == b.outer_position_pixels
        && a.fullscreen == b.fullscreen
        && a.maximized == b.maximized
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
                    open_file_placeholder(app);
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

fn open_file_placeholder(app: &mut HxyApp) {
    let bytes: Vec<u8> = (0u8..=255).collect();
    app.open_in_memory("sample", bytes);
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
