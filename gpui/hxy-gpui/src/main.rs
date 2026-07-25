use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use gpui::App;
use gpui::Bounds;
use gpui::WindowBounds;
use gpui::WindowOptions;
use gpui::prelude::*;
use gpui::px;
use gpui::size;
use gpui_component::Root;
use hxy_core::HexSource;
use hxy_core::MemorySource;

mod menu;
mod palette;
mod panels;
mod persist;
mod status;
mod workspace;

use workspace::Workspace;

fn main() -> ExitCode {
    // Full file-open UX also covers cmd-o (workspace.rs); a CLI path
    // argument opens the same way at startup.
    let initial = match std::env::args().nth(1) {
        Some(path) => match std::fs::read(&path) {
            Ok(bytes) => Some((Arc::new(MemorySource::new(bytes)) as Arc<dyn HexSource>, PathBuf::from(path))),
            Err(err) => {
                eprintln!("hxy-gpui: cannot read {path}: {err}");
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };

    gpui::Application::new().run(move |cx: &mut App| {
        gpui_component::init(cx);
        panels::register(cx);
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
                cx.new(|cx| Root::new(workspace, window, cx))
            },
        )
        .expect("open window");
        cx.activate(true);
    });
    ExitCode::SUCCESS
}
