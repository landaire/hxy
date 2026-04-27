#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result<()> {
    use std::sync::Arc;

    use hxy_lib::cli::Cli;
    use hxy_lib::ipc;
    use hxy_lib::settings::persist;
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
    let startup = load_persistent_state(&runtime);

    let state = shared(PersistedState {
        window: loaded_window.unwrap_or_default(),
        app: startup.app.unwrap_or_default(),
        open_tabs: startup.open_tabs.unwrap_or_default(),
        plugin_grants: startup.plugin_grants,
        dock_layout_json: startup.dock_layout_json,
        vfs_tree_expanded: startup.vfs_tree_expanded,
    });
    let sink = startup.sink;
    let plugin_state_store = startup.plugin_state_store;

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
            // Wire plugin persistence *before* the sink. The sink's
            // `restore_open_tabs` step may try to remount a plugin-
            // backed tab via `mount_by_token`; if the state store
            // isn't wired yet the plugin would see `denied` from
            // `state::load` and could misbehave.
            if let Some(state_store) = plugin_state_store {
                app = app.with_plugin_persistence(state_store);
            }
            if let Some(sink) = sink {
                app = app.with_sink(sink);
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

/// Everything [`main`] reads out of the SQLite-backed persistence
/// layer at startup. The fields are individually optional because
/// each row may be missing (first launch) or unreadable (corrupt
/// blob, schema drift); the shape itself is fixed so call sites
/// destructure by name rather than counting tuple positions.
#[cfg(not(target_arch = "wasm32"))]
struct StartupPersistence {
    sink: Option<hxy_lib::settings::persist::SaveSink>,
    plugin_state_store: Option<std::sync::Arc<dyn hxy_plugin_host::StateStore>>,
    app: Option<hxy_lib::settings::AppSettings>,
    open_tabs: Option<Vec<hxy_lib::state::OpenTabState>>,
    plugin_grants: hxy_plugin_host::PluginGrants,
    dock_layout_json: Option<String>,
    vfs_tree_expanded: Vec<(hxy_vfs::TabSource, Vec<String>)>,
}

#[cfg(not(target_arch = "wasm32"))]
fn load_persistent_state(runtime: &std::sync::Arc<tokio::runtime::Runtime>) -> StartupPersistence {
    use hxy_lib::settings::persist;
    use hxy_lib::settings::persist::SaveSink;

    let _guard = runtime.enter();
    let pool = match runtime.block_on(persist::open_db()) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "open settings database");
            return StartupPersistence {
                sink: None,
                plugin_state_store: None,
                app: None,
                open_tabs: None,
                plugin_grants: hxy_plugin_host::PluginGrants::default(),
                dock_layout_json: None,
                vfs_tree_expanded: Vec::new(),
            };
        }
    };
    let app = match runtime.block_on(persist::load_app_settings(&pool)) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "load app settings -- using defaults");
            None
        }
    };
    let open_tabs = match runtime.block_on(persist::load_open_tabs(&pool)) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "load open tabs -- starting empty");
            None
        }
    };
    let plugin_grants = match runtime.block_on(persist::load_plugin_grants(&pool)) {
        Ok(v) => v.unwrap_or_default(),
        Err(e) => {
            tracing::warn!(error = %e, "load plugin grants -- treating all as denied");
            hxy_plugin_host::PluginGrants::default()
        }
    };
    let dock_layout_json = match runtime.block_on(persist::load_dock_layout(&pool)) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "load dock layout -- starting with default");
            None
        }
    };
    let vfs_tree_expanded = match runtime.block_on(persist::load_vfs_tree_expanded(&pool)) {
        Ok(v) => v.unwrap_or_default(),
        Err(e) => {
            tracing::warn!(error = %e, "load vfs tree expansion -- starting empty");
            Vec::new()
        }
    };
    let state_store: std::sync::Arc<dyn hxy_plugin_host::StateStore> =
        std::sync::Arc::new(persist::SqliteStateStore::new(pool.clone(), std::sync::Arc::clone(runtime)));
    StartupPersistence {
        sink: Some(SaveSink::new(pool, std::sync::Arc::clone(runtime))),
        plugin_state_store: Some(state_store),
        app,
        open_tabs,
        plugin_grants,
        dock_layout_json,
        vfs_tree_expanded,
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {}
