use std::process::ExitCode;
use std::sync::Arc;

use gpui::App;
use gpui::Bounds;
use gpui::Context;
use gpui::Entity;
use gpui::Window;
use gpui::WindowBounds;
use gpui::WindowOptions;
use gpui::div;
use gpui::prelude::*;
use gpui::px;
use gpui::size;
use gpui_component::ActiveTheme;
use gpui_component::Root;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_view_gpui::HexPane;

struct Workspace {
    pane: Option<Entity<HexPane>>,
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div().size_full().bg(cx.theme().background);
        match &self.pane {
            Some(pane) => root.child(pane.clone()),
            None => root.child(hxy_i18n::t("gpui-shell-no-file")),
        }
    }
}

fn main() -> ExitCode {
    // Full file-open UX is a later task; for now a single path argument
    // opens a read-only in-memory view so the shell shows a grid.
    let source = match std::env::args().nth(1) {
        Some(path) => match std::fs::read(&path) {
            Ok(bytes) => Some(Arc::new(MemorySource::new(bytes)) as Arc<dyn HexSource>),
            Err(err) => {
                eprintln!("hxy-gpui: cannot read {path}: {err}");
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };

    gpui::Application::new().run(move |cx: &mut App| {
        gpui_component::init(cx);
        let bounds = Bounds::centered(None, size(px(1024.0), px(768.0)), cx);
        cx.open_window(
            WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() },
            move |window, cx| {
                gpui_component::Theme::sync_system_appearance(Some(window), cx);
                let pane = source.map(|source| cx.new(|cx| HexPane::new(source, cx)));
                let workspace = cx.new(|_| Workspace { pane });
                cx.new(|cx| Root::new(workspace, window, cx))
            },
        )
        .expect("open window");
        cx.activate(true);
    });
    ExitCode::SUCCESS
}
