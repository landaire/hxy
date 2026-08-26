//! A torn-off tab hosted in its own OS window, as a first-class dock
//! surface.
//!
//! gpui entities are app-global, so a panel entity renders fine in a
//! second window's [`DockArea`] while the main [`Workspace`] keeps its
//! subscriptions on that entity alive (visualizer requests, template
//! console logs) -- those are entity-to-entity, not window-scoped.
//!
//! A float window is a peer of the main window: it registers with the
//! workspace's surface registry so a tab dragged off any window can be
//! routed into it, it lets its own tabs be dragged back out (the same
//! window-global mouse-up watcher the main window uses), it closes itself
//! when its last tab leaves, and on an explicit close it hands any
//! remaining tabs back to the main window so nothing is lost.

use std::sync::Arc;

use gpui::AnyElement;
use gpui::Context;
use gpui::DispatchPhase;
use gpui::DragMoveEvent;
use gpui::Entity;
use gpui::IntoElement;
use gpui::MouseButton;
use gpui::MouseUpEvent;
use gpui::ParentElement;
use gpui::Pixels;
use gpui::Point;
use gpui::Render;
use gpui::Styled;
use gpui::Subscription;
use gpui::WeakEntity;
use gpui::Window;
use gpui::WindowId;
use gpui::canvas;
use gpui::div;
use gpui::prelude::*;
use gpui::px;
use gpui::component::Root;
use gpui::component::dock::BasePanelView as PanelView;
use gpui::component::dock::DockArea;
use gpui::component::dock::DockEvent;
use gpui::component::dock::DockLayout;
use gpui::component::dock::DockPlacement;
use gpui::component::dock::DockSkin;
use gpui::component::dock::DragPanel;
use gpui::component::dock::PanelId;

use crate::workspace::Workspace;

/// A second-window dock surface, coordinated by the main [`Workspace`].
pub struct FloatingWindow {
    workspace: WeakEntity<Workspace>,
    dock: Entity<DockArea>,
    window_id: WindowId,
    /// The tab currently being dragged out of this window (tracked from
    /// `on_drag_move`, since the window-global mouse-up carries no drag
    /// payload). Cleared on every left mouse-up.
    dragging_tab: Option<PanelId>,
    _layout: Subscription,
    _release: Subscription,
}

impl FloatingWindow {
    pub fn new(
        panel: Arc<dyn PanelView>,
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
        let window_id = window.window_handle().window_id();

        // Register as a routable surface for cross-window tab drops.
        // Deferred: this window is often built inside the main workspace's
        // own update (a tab dragged/torn off it), and registering
        // immediately would re-enter that update and double-lease it.
        if let Some(ws) = workspace.upgrade() {
            let handle = window.window_handle();
            let weak_dock = dock.downgrade();
            window.defer(cx, move |_window, cx| {
                let _ = ws.update(cx, |ws, _| ws.register_float(handle, weak_dock));
            });
        }

        // Close this window once its dock goes empty -- i.e. its last tab
        // was dragged out into another window.
        let layout = cx.subscribe_in(&dock, window, |_float, dock, event: &DockEvent, window, cx| {
            if matches!(event, DockEvent::LayoutChanged) && dock.read(cx).is_empty(DockPlacement::Center, cx) {
                window.remove_window();
            }
        });

        // On close (the user closed a window that still held tabs): hand
        // the survivors back to the main window and drop from the registry.
        let ws_for_release = workspace.clone();
        let release = cx.on_release(move |this, cx| {
            if let Some(ws) = ws_for_release.upgrade() {
                let survivors = this.dock_panels(cx);
                ws.update(cx, |ws, cx| {
                    ws.reclaim_panels(survivors, cx);
                    ws.unregister_float(window_id);
                });
            }
        });

        Self { workspace, dock, window_id, dragging_tab: None, _layout: layout, _release: release }
    }

    /// Every live panel `Arc` in this window's dock (for reclaim-on-close).
    fn dock_panels(&self, cx: &gpui::App) -> Vec<Arc<dyn PanelView>> {
        let dock = self.dock.read(cx);
        dock.layout(DockPlacement::Center)
            .map(|tree| tree.panels().filter_map(|id| dock.panel(id).cloned()).collect())
            .unwrap_or_default()
    }

    /// Track which tab is being dragged out of this window.
    fn on_tab_drag_move(&mut self, event: &DragMoveEvent<DragPanel>, _window: &mut Window, cx: &mut Context<Self>) {
        self.dragging_tab = Some(event.drag(cx).panel());
    }

    /// A left mouse-up released outside this window routes the dragged tab
    /// through the main workspace (into another window, or a new float);
    /// an in-window release lets this dock's own drop stand.
    fn on_drag_release(&mut self, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.dragging_tab.take() else { return };
        let size = window.viewport_size();
        let outside =
            position.x < px(0.0) || position.y < px(0.0) || position.x > size.width || position.y > size.height;
        if !outside {
            return;
        }
        let global = window.bounds().origin + position;
        let source_id = self.window_id;
        let source_dock = self.dock.clone();
        if let Some(ws) = self.workspace.upgrade() {
            ws.update(cx, |ws, cx| ws.route_dragged_tab(source_dock, source_id, id, global, window, cx));
        }
    }

    /// Registers a window-global mouse-up listener each frame so a release
    /// outside every element's hitbox is observed (mirrors the main
    /// window's watcher). Paints nothing.
    fn render_drag_release_watcher(&self, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        canvas(
            |_, _, _| (),
            move |_, (), window, _cx| {
                window.on_mouse_event::<MouseUpEvent>(move |event, phase, window, cx| {
                    if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                        return;
                    }
                    let position = event.position;
                    _ = this.update(cx, |float, cx| float.on_drag_release(position, window, cx));
                });
            },
        )
        .absolute()
        .size_full()
        .into_any_element()
    }
}

impl Render for FloatingWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Like `Workspace::render`, this view is a `Root` child, so it
        // appends the dialog/notification layers itself.
        div()
            .size_full()
            .on_drag_move(cx.listener(Self::on_tab_drag_move))
            .child(self.dock.clone())
            .child(self.render_drag_release_watcher(cx))
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}
