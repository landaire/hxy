#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result<()> {
    use std::sync::Arc;

    use hxy_lib::cli::Cli;
    use hxy_lib::ipc;
    use hxy_lib::persist;
    use hxy_lib::persist::SaveSink;
    use hxy_lib::state::PersistedState;
    use hxy_lib::state::shared;
    use tokio::runtime::Runtime;
    use tracing_subscriber::EnvFilter;

    let filter = match EnvFilter::try_from_default_env() {
        Ok(f) => f,
        Err(_) => EnvFilter::new("info"),
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    hxy_i18n::init_from_system_locale();

    let cli = Cli::parse_args();
    let cli_files = cli.resolved_files();
    // Single-instance: if another hxy is already running, hand off
    // the file list and exit so the user sees their tabs in the
    // window they already had open instead of a brand-new copy of
    // the app stealing focus. A failed connect is the normal
    // "we're the first instance" path -- keep going and start the
    // GUI ourselves.
    if !cli_files.is_empty() {
        match ipc::try_send_to_running_instance(&cli_files) {
            Ok(()) => {
                tracing::info!(count = cli_files.len(), "forwarded to running instance");
                return Ok(());
            }
            Err(e) => {
                tracing::debug!(error = %e, "no running instance; starting our own");
            }
        }
    }

    let loaded_window = match persist::load_window_settings_sync() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "load window settings -- using defaults");
            None
        }
    };

    let runtime = Arc::new(Runtime::new().expect("create tokio runtime"));
    let (sink, loaded_app, loaded_tabs, plugin_persistence) = {
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
                let grants = match runtime.block_on(persist::load_plugin_grants(&pool)) {
                    Ok(v) => v.unwrap_or_default(),
                    Err(e) => {
                        tracing::warn!(error = %e, "load plugin grants -- treating all as denied");
                        hxy_plugin_host::PluginGrants::default()
                    }
                };
                let state_store: Arc<dyn hxy_plugin_host::StateStore> =
                    Arc::new(persist::SqliteStateStore::new(pool.clone(), Arc::clone(&runtime)));
                (
                    Some(SaveSink::new(pool, Arc::clone(&runtime))),
                    app_settings,
                    tabs,
                    Some((grants, state_store)),
                )
            }
            Err(e) => {
                tracing::warn!(error = %e, "open settings database");
                (None, None, None, None)
            }
        }
    };

    let state = shared(PersistedState {
        window: loaded_window.unwrap_or_default(),
        app: loaded_app.unwrap_or_default(),
        open_tabs: loaded_tabs.unwrap_or_default(),
    });

    let viewport = egui::ViewportBuilder::default()
        .with_title(hxy_lib::APP_NAME)
        .with_min_inner_size([480.0, 320.0])
        .with_drag_and_drop(true);
    let viewport = state.read().window.apply_to_builder(viewport, [1200.0, 800.0]);

    let options = eframe::NativeOptions { viewport, ..Default::default() };

    let state_for_app = Arc::clone(&state);

    eframe::run_native(
        hxy_lib::APP_NAME,
        options,
        Box::new(move |cc| {
            let mut app = hxy_lib::HxyApp::new(cc, state_for_app);
            if let Some(sink) = sink {
                app = app.with_sink(sink);
            }
            if let Some((grants, state_store)) = plugin_persistence {
                app = app.with_plugin_persistence(grants, state_store);
            }
            // The IPC inbox needs the egui Context to schedule
            // repaints when a forwarded batch arrives, so spin up
            // the listener here -- after `cc` is available -- and
            // hand the inbox to the app.
            if let Some(inbox) = ipc::start_server(&cc.egui_ctx) {
                app = app.with_ipc_inbox(inbox);
            }
            if !cli_files.is_empty() {
                app = app.with_cli_paths(cli_files);
            }
            Ok(Box::new(app))
        }),
    )
}

#[cfg(target_arch = "wasm32")]
fn main() {}
