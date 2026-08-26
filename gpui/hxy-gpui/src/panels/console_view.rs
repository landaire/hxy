//! [`ConsolePanel`]: the dockable Console tab. Renders the app-global
//! [`ConsoleLogGlobal`] the workspace publishes on each `console_log`,
//! mirroring the egui `console_ui`
//! (`crates/hxy/src/app/mod.rs`): an empty-state placeholder, else a
//! scrollable list of `time / severity-icon / context / message` rows
//! with per-severity theme colors.
//!
//! A workspace-scoped singleton like the settings and plugins panels:
//! it carries no per-instance state (it reads the live global), so it
//! restores fresh and its layout entry is `PanelKind::AlwaysKeep`. The
//! panel holds no handle back to the workspace; it observes the global
//! and re-renders when a new entry lands.

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
use gpui::StatefulInteractiveElement;
use gpui::Styled;
use gpui::Subscription;
use gpui::Window;
use gpui::div;
use gpui::px;
use gpui::component::ActiveTheme;
use gpui::component::Icon;
use gpui::component::IconName;
use gpui::component::Sizable;
use gpui::component::dock::BasePanel;
use gpui::component::dock::Panel;
use gpui::component::dock::PanelEvent;
use gpui::component::h_flex;
use gpui::component::v_flex;

use crate::console::ConsoleLogGlobal;
use crate::console::ConsoleSeverity;

/// Stable identifier for layout (de)serialization; must never change.
pub const CONSOLE_PANEL_NAME: &str = "ConsolePanel";

pub struct ConsolePanel {
    focus_handle: FocusHandle,
    _sub: Subscription,
}

impl ConsolePanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // A new entry republishes the global; re-render to show it.
        let sub = cx.observe_global::<ConsoleLogGlobal>(|_, cx| cx.notify());
        Self { focus_handle: cx.focus_handle(), _sub: sub }
    }
}

/// Compact wall-clock time for a console row, `HH:MM:SS` in UTC.
/// Mirrors egui's `format_console_time`, which zones to UTC despite its
/// "user-local" comment; kept identical so both frontends read alike.
fn format_console_time(ts: jiff::Timestamp) -> String {
    let zoned = ts.to_zoned(jiff::tz::TimeZone::UTC);
    format!("{:02}:{:02}:{:02}", zoned.hour(), zoned.minute(), zoned.second())
}

impl BasePanel for ConsolePanel {
    fn panel_name(&self) -> &'static str {
        CONSOLE_PANEL_NAME
    }
}

impl Panel for ConsolePanel {
    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        SharedString::from(hxy_i18n::t("tab-console"))
    }

    fn tab_name(&self, _cx: &App) -> Option<SharedString> {
        Some(SharedString::from(hxy_i18n::t("tab-console")))
    }
}

impl Focusable for ConsolePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ConsolePanel {}

impl Render for ConsolePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Empty until the first `console_log`; an absent global renders
        // the empty-state placeholder, so the default is correct here.
        let entries = cx.try_global::<ConsoleLogGlobal>().map(|g| g.0.clone()).unwrap_or_default();
        let root = div().id("console-panel").track_focus(&self.focus_handle).size_full().overflow_y_scroll();
        if entries.is_empty() {
            return root
                .flex()
                .items_center()
                .justify_center()
                .child(div().text_color(cx.theme().muted_foreground).child(hxy_i18n::t("console-empty")));
        }

        let mono = cx.theme().mono_font_family.clone();
        let muted = cx.theme().muted_foreground;
        let mut list = v_flex().p_2().gap_0p5();
        // Entries are stored oldest-first; rendering in order puts the
        // newest at the bottom. Unlike egui's stick-to-bottom ScrollArea
        // the view does not autoscroll to a fresh entry -- the user
        // scrolls to it (a minor deviation).
        for entry in &entries {
            let (icon, color) = match entry.severity {
                ConsoleSeverity::Info => (IconName::Info, muted),
                ConsoleSeverity::Warning => (IconName::TriangleAlert, cx.theme().warning),
                ConsoleSeverity::Error => (IconName::CircleX, cx.theme().danger),
            };
            list = list.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(px(72.0))
                            .flex_none()
                            .font_family(mono.clone())
                            .text_color(muted)
                            .child(format_console_time(entry.timestamp)),
                    )
                    .child(Icon::new(icon).xsmall().text_color(color))
                    .child(div().flex_none().text_color(muted).child(entry.context.clone()))
                    .child(div().flex_1().child(entry.message.clone())),
            );
        }
        root.child(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_console_time_is_zero_padded_utc_hms() {
        // 1970-01-01T00:00:05Z -> "00:00:05".
        let ts = jiff::Timestamp::from_second(5).expect("timestamp");
        assert_eq!(format_console_time(ts), "00:00:05");
        // 13:02:09 UTC on the same day.
        let ts = jiff::Timestamp::from_second(13 * 3600 + 2 * 60 + 9).expect("timestamp");
        assert_eq!(format_console_time(ts), "13:02:09");
    }
}
