//! [`MemoryPanel`]: the dockable Memory tab.
//!
//! The egui Memory panel debugs `hxy_core::ByteCache` occupancy. This app
//! has no `ByteCache` -- every file opens whole into a `MemorySource` -- so
//! the gpui panel shows the equivalent for this memory model: one row per
//! open file with its resident byte count. Empty when no file is open.

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
use gpui::WeakEntity;
use gpui::Window;
use gpui::div;
use gpui::component::ActiveTheme;
use gpui::component::dock::BasePanel;
use gpui::component::dock::Panel;
use gpui::component::dock::PanelEvent;
use gpui::component::h_flex;
use gpui::component::v_flex;

use crate::workspace::Workspace;

/// Stable identifier for layout (de)serialization; must never change.
pub const MEMORY_PANEL_NAME: &str = "MemoryPanel";

pub struct MemoryPanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    _sub: Option<Subscription>,
}

impl MemoryPanel {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        // Re-render when the workspace changes (a file opens or closes).
        let sub = workspace.upgrade().map(|ws| cx.observe(&ws, |_, _, cx| cx.notify()));
        Self { focus_handle: cx.focus_handle(), workspace, _sub: sub }
    }
}

fn mib_of(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

impl BasePanel for MemoryPanel {
    fn panel_name(&self) -> &'static str {
        MEMORY_PANEL_NAME
    }
}

impl Panel for MemoryPanel {
    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        SharedString::from(hxy_i18n::t("tab-memory"))
    }

    fn tab_name(&self, _cx: &App) -> Option<SharedString> {
        Some(SharedString::from(hxy_i18n::t("tab-memory")))
    }
}

impl Focusable for MemoryPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for MemoryPanel {}

impl Render for MemoryPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.workspace.upgrade().map(|ws| ws.read(cx).open_file_memory(cx)).unwrap_or_default();
        let root = div().id("memory-panel").track_focus(&self.focus_handle).size_full().overflow_y_scroll();
        if rows.is_empty() {
            return root
                .flex()
                .items_center()
                .justify_center()
                .child(div().text_color(cx.theme().muted_foreground).child(hxy_i18n::t("memory-panel-empty")));
        }

        let mono = cx.theme().mono_font_family.clone();
        let muted = cx.theme().muted_foreground;
        let mut list = v_flex().p_2().gap_0p5();
        for (name, bytes) in &rows {
            list = list.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().child(name.clone()))
                    .child(
                        div()
                            .flex_none()
                            .font_family(mono.clone())
                            .text_color(muted)
                            .child(hxy_i18n::t_args("memory-panel-bytes-mib", &[("mib", &format!("{:.1}", mib_of(*bytes)))])),
                    ),
            );
        }
        root.child(list)
    }
}
