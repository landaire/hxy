#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result<()> {
    use std::sync::Arc;
    use std::time::Duration;

    use hxy_app::persist;
    use hxy_app::state::PersistedState;
    use hxy_app::state::shared;
    use tokio::runtime::Runtime;
    use tracing_subscriber::EnvFilter;

    let filter = match EnvFilter::try_from_default_env() {
        Ok(f) => f,
        Err(_) => EnvFilter::new("info"),
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    hxy_i18n::init_from_system_locale();

    let loaded_window = persist::load_window_settings_sync();

    let runtime = Arc::new(Runtime::new().expect("create tokio runtime"));
    let (pool_result, loaded_app) = runtime.block_on(async {
        match persist::open_db().await {
            Ok(pool) => {
                let app = match persist::load_app_settings(&pool).await {
                    Ok(opt) => opt,
                    Err(e) => {
                        tracing::warn!(error = %e, "load app settings — using defaults");
                        None
                    }
                };
                (Some(pool), app)
            }
            Err(e) => {
                tracing::warn!(error = %e, "open settings database");
                (None, None)
            }
        }
    });

    let state =
        shared(PersistedState { window: loaded_window.unwrap_or_default(), app: loaded_app.unwrap_or_default() });

    let save_handle = pool_result.map(|pool| {
        let _guard = runtime.enter();
        persist::spawn_save_task(pool, Arc::clone(&state), Duration::from_secs(30))
    });

    let viewport = egui::ViewportBuilder::default()
        .with_title(hxy_app::APP_NAME)
        .with_min_inner_size([480.0, 320.0])
        .with_drag_and_drop(true);
    let viewport = state.read().window.apply_to_builder(viewport, [1200.0, 800.0]);

    let options = eframe::NativeOptions { viewport, ..Default::default() };

    let state_for_app = Arc::clone(&state);
    let save_notify = save_handle.as_ref().map(|h| Arc::clone(&h.notify));

    let result = eframe::run_native(
        hxy_app::APP_NAME,
        options,
        Box::new(move |cc| {
            let mut app = hxy_app::HxyApp::new(cc, state_for_app);
            if let Some(notify) = save_notify {
                app = app.with_save_notify(notify);
            }
            Ok(Box::new(app))
        }),
    );

    if let Some(handle) = save_handle {
        runtime.block_on(handle.shutdown());
    }

    result
}

#[cfg(target_arch = "wasm32")]
fn main() {}
