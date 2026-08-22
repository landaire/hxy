//! [`WelcomePanel`]: the placeholder shown when the workspace has no
//! open file tabs, with the recent-files list (mirrors egui's
//! `welcome_ui`).

use std::path::PathBuf;

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
use gpui_component::ActiveTheme;
use gpui_component::Icon;
use gpui_component::IconName;
use gpui_component::button::Button;
use gpui_component::button::ButtonVariants;
use gpui_component::dock::BasePanel;
use gpui_component::dock::Panel;
use gpui_component::dock::PanelEvent;
use gpui_component::v_flex;
use hxy_settings::RecentFile;

use crate::settings::SettingsGlobal;

/// Stable identifier for layout (de)serialization; must never change.
pub const WELCOME_PANEL_NAME: &str = "WelcomePanel";

/// Cap on the recents scroll area, so a full 20-entry list scrolls
/// instead of pushing the headings off-center.
const RECENTS_MAX_HEIGHT: f32 = 240.0;

/// A recent-files row was clicked; the workspace opens the carried
/// path through the normal open flow (mirrors egui's
/// `WELCOME_OPEN_RECENT` temp-data queue, as a typed event instead).
#[derive(Clone, Debug)]
pub struct OpenRecentRequested(pub PathBuf);

impl EventEmitter<OpenRecentRequested> for WelcomePanel {}

pub struct WelcomePanel {
    focus_handle: FocusHandle,
    /// Repaints the recents list when the settings global changes
    /// (a fresh open reorders it).
    _settings_observe: Subscription,
}

impl WelcomePanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let settings_observe = cx.observe_global::<SettingsGlobal>(|_, cx| cx.notify());
        Self { focus_handle: cx.focus_handle(), _settings_observe: settings_observe }
    }
}

/// One rendered recents row: the file-name label shown on the button
/// and the full path it opens (also the hover tooltip). Pure so the
/// list shape is unit-testable without a window.
pub(crate) fn recent_rows(recents: &[RecentFile]) -> Vec<(String, PathBuf)> {
    recents
        .iter()
        .map(|entry| {
            let label = entry
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                // A path without a final component (trailing `..` or
                // separator) still identifies a real recent entry;
                // show the whole path rather than a blank row.
                .unwrap_or_else(|| entry.path.display().to_string());
            (label, entry.path.clone())
        })
        .collect()
}

impl BasePanel for WelcomePanel {
    fn panel_name(&self) -> &'static str {
        WELCOME_PANEL_NAME
    }

    /// The welcome tab is managed by the workspace, not the user, so it
    /// carries no close affordance.
    fn closable(&self, _cx: &App) -> bool {
        false
    }
}

impl Panel for WelcomePanel {
    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        SharedString::from(hxy_i18n::t("gpui-welcome-title"))
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
        let settings = crate::settings::settings(cx);
        let rows = recent_rows(&settings.recent_files);

        let recents: gpui::AnyElement = if rows.is_empty() {
            div().text_color(cx.theme().muted_foreground).child(hxy_i18n::t("welcome-recent-empty")).into_any_element()
        } else {
            v_flex()
                .id("welcome-recents")
                .gap_1()
                .items_center()
                .max_h(px(RECENTS_MAX_HEIGHT))
                .overflow_y_scroll()
                .children(rows.into_iter().enumerate().map(|(i, (label, path))| {
                    let tooltip = path.display().to_string();
                    Button::new(("welcome-recent", i))
                        .ghost()
                        .compact()
                        .icon(Icon::new(IconName::File))
                        .label(label)
                        .tooltip(tooltip)
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(OpenRecentRequested(path.clone()));
                        }))
                }))
                .into_any_element()
        };

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
            .child(div().mt_4().text_color(cx.theme().foreground).child(hxy_i18n::t("welcome-recent")))
            .child(recents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recent(path: &str) -> RecentFile {
        RecentFile { path: PathBuf::from(path), last_opened: jiff::Timestamp::UNIX_EPOCH }
    }

    #[test]
    fn recent_rows_label_is_the_file_name_and_path_is_kept() {
        let rows = recent_rows(&[recent("/a/b/dump.bin"), recent("/tmp/x.hex")]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], ("dump.bin".to_string(), PathBuf::from("/a/b/dump.bin")));
        assert_eq!(rows[1].0, "x.hex");
    }

    #[test]
    fn recent_rows_without_file_name_fall_back_to_full_path() {
        let rows = recent_rows(&[recent("/a/b/..")]);
        assert_eq!(rows[0].0, PathBuf::from("/a/b/..").display().to_string());
    }

    #[test]
    fn recent_rows_empty_list_is_empty() {
        assert!(recent_rows(&[]).is_empty());
    }
}
