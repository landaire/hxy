//! [`WelcomePanel`]: the placeholder shown when the workspace has no
//! open file tabs.

use gpui::App;
use gpui::Context;
use gpui::EventEmitter;
use gpui::FocusHandle;
use gpui::Focusable;
use gpui::InteractiveElement;
use gpui::IntoElement;
use gpui::ParentElement;
use gpui::Render;
use gpui::SharedString;
use gpui::Styled;
use gpui::Window;
use gpui::div;
use gpui_component::ActiveTheme;
use gpui_component::dock::Panel;
use gpui_component::dock::PanelEvent;

/// Stable identifier for layout (de)serialization; must never change.
pub const WELCOME_PANEL_NAME: &str = "WelcomePanel";

pub struct WelcomePanel {
    focus_handle: FocusHandle,
}

impl WelcomePanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self { focus_handle: cx.focus_handle() }
    }
}

impl Panel for WelcomePanel {
    fn panel_name(&self) -> &'static str {
        WELCOME_PANEL_NAME
    }

    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        SharedString::from(hxy_i18n::t("gpui-welcome-title"))
    }

    /// The welcome tab is managed by the workspace, not the user, so it
    /// carries no close affordance.
    fn closable(&self, _cx: &App) -> bool {
        false
    }
}

impl Focusable for WelcomePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for WelcomePanel {}

impl Render for WelcomePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .text_color(cx.theme().muted_foreground)
            .child(hxy_i18n::t("gpui-welcome-title"))
            .child(hxy_i18n::t("gpui-welcome-body"))
    }
}
