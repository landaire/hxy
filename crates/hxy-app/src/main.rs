#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result<()> {
    use tracing_subscriber::EnvFilter;

    let filter = match EnvFilter::try_from_default_env() {
        Ok(f) => f,
        Err(_) => EnvFilter::new("info"),
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    hxy_i18n::init_from_system_locale();

    let settings = hxy_app::settings::AppSettings::default();

    let viewport = egui::ViewportBuilder::default()
        .with_title(hxy_app::APP_NAME)
        .with_min_inner_size([480.0, 320.0])
        .with_inner_size([1200.0, 800.0])
        .with_drag_and_drop(true);

    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(hxy_app::APP_NAME, options, Box::new(move |cc| Ok(Box::new(hxy_app::HxyApp::new(cc, settings)))))
}

#[cfg(target_arch = "wasm32")]
fn main() {}
