use std::process::ExitCode;

use gpui::App;
use gpui::Bounds;
use gpui::WindowBounds;
use gpui::WindowOptions;
use gpui::prelude::*;
use gpui::px;
use gpui::size;
use gpui_component::Root;
use gpui_component::WindowExt;

mod assets;
mod menu;
mod palette;
mod panels;
mod patches;
mod persist;
mod plugins;
mod settings;
mod status;
mod templates;
mod theme;
mod watch;
mod workspace;

use workspace::Workspace;

fn main() -> ExitCode {
    // macOS Finder "Open With" / right-click "Open in hxy": register the
    // Apple-Event / NSServices handlers BEFORE `Application::run` starts
    // AppKit, or a cold-start open-with document crashes the delegate
    // state machine mid-dispatch (see `hxy_ipc::macos_open::install`).
    #[cfg(target_os = "macos")]
    hxy_ipc::macos_open::install();

    // Full file-open UX also covers cmd-o (workspace.rs); every CLI path
    // argument opens the same way at startup (read, dedup, error-toast
    // on failure -- see `Workspace::build_initial`), so all of them open
    // as tabs, not just the first. `resolved_files` canonicalizes against
    // the CWD and drops missing paths so a forwarded path is CWD-neutral.
    let initial = hxy_ipc::cli::Cli::parse_args().resolved_files();

    // Single-instance: if another hxy is already running, hand it the file
    // list and exit WITHOUT opening a second window. An empty list means a
    // bare re-launch, which starts its own window (mirrors the egui app);
    // a failed connect is the normal "we are the first instance" path.
    if !initial.is_empty() && hxy_ipc::socket::try_send_to_running_instance(&initial).is_ok() {
        tracing::info!(count = initial.len(), "forwarded to running instance");
        return ExitCode::SUCCESS;
    }

    // Blocking pre-window load, like egui's `load_persistent_state`:
    // the workspace and its panels read settings at construction, so
    // the global must exist before the window opens.
    let boot = settings::load_blocking();
    let settings_failure = boot.failure;

    gpui::Application::new().with_assets(assets::Assets).run(move |cx: &mut App| {
        // Bind the single-instance socket and hand the workspace a receiver
        // of forwarded path batches. A bind failure (stale lock, perms)
        // leaves the app running without accepting forwarded opens.
        let ipc_receiver = match hxy_ipc::socket::start_server() {
            Ok(rx) => Some(rx),
            Err(e) => {
                tracing::warn!(error = %e, "ipc: bind socket; forwarded opens disabled");
                None
            }
        };
        gpui_component::init(cx);
        // Must run after gpui_component::init (Theme global) and before
        // the window's sync_system_appearance so the first paint and all
        // later appearance toggles use the hxy theme pair.
        theme::init(cx);
        settings::init(cx, boot);
        // Reads the shared persist handle settings just installed, so
        // it must follow settings::init and precede the window (panels
        // and the palette read the plugin globals).
        plugins::init(cx);
        panels::register(cx);
        cx.set_global(templates::load_runtimes());
        cx.set_global(templates::load_library());
        workspace::init_keybindings(cx);
        menu::init_keybindings(cx);
        menu::init_global_actions(cx);
        cx.set_menus(menu::build_menus());
        let layout_path = persist::layout_path();
        let bounds = Bounds::centered(None, size(px(1024.0), px(768.0)), cx);
        cx.open_window(
            WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() },
            move |window, cx| {
                gpui_component::Theme::sync_system_appearance(Some(window), cx);
                let appearance_subscription = window.observe_window_appearance(|window, cx| {
                    gpui_component::Theme::sync_system_appearance(Some(window), cx);
                });
                // Initial keyboard focus (active pane if a file loaded,
                // otherwise the workspace itself so cmd-o stays
                // reachable) is assigned by `Workspace`'s own first
                // render -- see its `focus_pending` field.
                //
                // gpui-component's Root must be the window's top-level
                // view (it manages the dialog/sheet/notification
                // layers); its render shows only this child, which is
                // all Task 2 needs -- no modal surfaces yet (toasts are
                // Task 6).
                let workspace = cx.new(|cx| Workspace::new(initial, appearance_subscription, layout_path, window, cx));
                // Route forwarded second-instance / macOS open-with paths
                // into this running workspace via an executor poll loop. The
                // receiver is `None` when the socket failed to bind; on
                // macOS the loop still drains the Apple-Event buffer, so it
                // must not be gated on the socket (mirrors egui, which
                // drains that buffer every frame regardless).
                workspace.update(cx, |ws, cx| ws.start_ipc(ipc_receiver, window, cx));
                if let Some(failure) = settings_failure {
                    // Deferred like the boot-restore toasts in
                    // `Workspace::build_initial`: the Root notification
                    // layer does not exist until after this closure
                    // returns.
                    window.defer(cx, move |window, cx| {
                        let text = hxy_i18n::t(failure.toast_key());
                        window.push_notification(gpui_component::notification::Notification::warning(text), cx);
                    });
                }
                cx.new(|cx| Root::new(workspace, window, cx))
            },
        )
        .expect("open window");
        cx.activate(true);
    });
    ExitCode::SUCCESS
}
