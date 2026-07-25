use gpui::App;
use gpui::Bounds;
use gpui::Context;
use gpui::Window;
use gpui::WindowBounds;
use gpui::WindowOptions;
use gpui::div;
use gpui::prelude::*;
use gpui::px;
use gpui::size;
use gpui_component::ActiveTheme;
use gpui_component::Root;

struct Workspace;

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().bg(cx.theme().background).child(hxy_i18n::t("gpui-shell-no-file"))
    }
}

fn main() {
    gpui::Application::new().run(|cx: &mut App| {
        gpui_component::init(cx);
        let bounds = Bounds::centered(None, size(px(1024.0), px(768.0)), cx);
        cx.open_window(
            WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() },
            |window, cx| {
                gpui_component::Theme::sync_system_appearance(Some(window), cx);
                let workspace = cx.new(|_| Workspace);
                cx.new(|cx| Root::new(workspace, window, cx))
            },
        )
        .expect("open window");
        cx.activate(true);
    });
}
