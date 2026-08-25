//! A torn-off tab hosted in its own OS window.
//!
//! gpui entities are app-global, so a panel entity renders fine in a
//! second window's [`DockArea`] while the main [`Workspace`] keeps its
//! subscriptions on that entity alive (visualizer requests, template
//! console logs) -- those are entity-to-entity, not window-scoped.
//!
//! The main workspace holds the torn panel's `Arc` in its `torn_off`
//! registry, so the entity outlives this window regardless of which dock
//! references it. Closing the window drops this view, whose `on_release`
//! hands the panel back to the main dock -- so unsaved edits (which live
//! in the panel entity itself) are never lost by tearing off.

use std::sync::Arc;

use gpui::Context;
use gpui::Entity;
use gpui::IntoElement;
use gpui::ParentElement;
use gpui::Render;
use gpui::Styled;
use gpui::Subscription;
use gpui::WeakEntity;
use gpui::Window;
use gpui::div;
use gpui::prelude::*;
use gpui_component::Root;
use gpui_component::dock::BasePanelView as PanelView;
use gpui_component::dock::DockArea;
use gpui_component::dock::DockLayout;
use gpui_component::dock::DockSkin;
use gpui_component::dock::PanelId;

use crate::workspace::Workspace;

/// A second-window host for one torn-off panel: a bare [`DockArea`]
/// holding the panel as its sole center tab, plus the reclaim-on-close
/// hook that returns it to the main window.
pub struct FloatingWindow {
    dock: Entity<DockArea>,
    _release: Subscription,
}

impl FloatingWindow {
    pub fn new(
        panel: Arc<dyn PanelView>,
        panel_id: PanelId,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let dock = cx.new(|cx| {
            let mut dock = DockArea::new("hxy-floating", None, window, cx).with_renderer(DockSkin::new(cx));
            dock.set_center(DockLayout::tabs().panel_view(panel.clone(), cx), window, cx);
            dock
        });
        window.focus(&panel.focus_handle(cx), cx);
        // The window closing drops this view; hand the panel back to the
        // main workspace, which still owns the `Arc` in `torn_off` (so the
        // entity, and its unsaved edits, are intact to reclaim).
        let release = cx.on_release(move |_this, cx| {
            if let Some(workspace) = workspace.upgrade() {
                workspace.update(cx, |workspace, cx| workspace.reclaim_torn_panel(panel_id, cx));
            }
        });
        Self { dock, _release: release }
    }
}

impl Render for FloatingWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Like `Workspace::render`, this view is a `Root` child, so it
        // appends the dialog/notification layers itself.
        div()
            .size_full()
            .child(self.dock.clone())
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}
