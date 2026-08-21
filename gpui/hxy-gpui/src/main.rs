use std::path::PathBuf;
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
mod settings;
mod status;
mod templates;
mod watch;
mod workspace;

use workspace::Workspace;

fn main() -> ExitCode {
    // Full file-open UX also covers cmd-o (workspace.rs); every CLI path
    // argument opens the same way at startup (read, dedup, error-toast
    // on failure -- see `Workspace::build_initial`), so all of them open
    // as tabs, not just the first.
    let initial: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();

    // Blocking pre-window load, like egui's `load_persistent_state`:
    // the workspace and its panels read settings at construction, so
    // the global must exist before the window opens.
    let boot = settings::load_blocking();
    let settings_failure = boot.failure;

    gpui::Application::new().with_assets(assets::Assets).run(move |cx: &mut App| {
        gpui_component::init(cx);
        settings::init(cx, boot);
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
