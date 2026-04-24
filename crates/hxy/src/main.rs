#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result<()> {
    use std::sync::Arc;

    use hxy::persist;
    use hxy::persist::SaveSink;
    use hxy::state::PersistedState;
    use hxy::state::shared;
    use tokio::runtime::Runtime;
    use tracing_subscriber::EnvFilter;

    let filter = match EnvFilter::try_from_default_env() {
        Ok(f) => f,
        Err(_) => EnvFilter::new("info"),
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    hxy_i18n::init_from_system_locale();

    let loaded_window = match persist::load_window_settings_sync() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "load window settings -- using defaults");
            None
        }
    };

    let runtime = Arc::new(Runtime::new().expect("create tokio runtime"));
    let (sink, loaded_app, loaded_tabs) = {
        let _guard = runtime.enter();
        match runtime.block_on(persist::open_db()) {
            Ok(pool) => {
                let app_settings = match runtime.block_on(persist::load_app_settings(&pool)) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(error = %e, "load app settings -- using defaults");
                        None
                    }
                };
                let tabs = match runtime.block_on(persist::load_open_tabs(&pool)) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(error = %e, "load open tabs -- starting empty");
                        None
                    }
                };
                (Some(SaveSink::new(pool, Arc::clone(&runtime))), app_settings, tabs)
            }
            Err(e) => {
                tracing::warn!(error = %e, "open settings database");
                (None, None, None)
            }
        }
    };

    let state = shared(PersistedState {
        window: loaded_window.unwrap_or_default(),
        app: loaded_app.unwrap_or_default(),
        open_tabs: loaded_tabs.unwrap_or_default(),
    });

    let viewport = egui::ViewportBuilder::default()
        .with_title(hxy::APP_NAME)
        .with_min_inner_size([480.0, 320.0])
        .with_drag_and_drop(true);
    let viewport = state.read().window.apply_to_builder(viewport, [1200.0, 800.0]);

    let options = eframe::NativeOptions { viewport, ..Default::default() };

    let state_for_app = Arc::clone(&state);

    eframe::run_native(
        hxy::APP_NAME,
        options,
        Box::new(move |cc| {
            let mut app = hxy::HxyApp::new(cc, state_for_app);
            if let Some(sink) = sink {
                app = app.with_sink(sink);
            }
            Ok(Box::new(app))
        }),
    )
}

#[cfg(target_arch = "wasm32")]
fn main() {}
