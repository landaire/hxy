//! [`WorkspaceHostPanel`]: a dock [`Panel`] that wraps and owns its own
//! inner [`DockArea`], so a VFS mount can live inside a single outer tab
//! as a full nested workspace (a VFS tree plus the entries opened from
//! it). Promoted from the M2 nested-dock spike; the mechanism it relies
//! on -- a `Panel` childing an `Entity<DockArea>`, with the inner layout
//! hand-composed into the wrapper's [`PanelInfo::Panel`] json -- is the
//! one the spike verdict validated end-to-end
//! (`docs/superpowers/plans/2026-07-25-m2-nested-dock-verdict.md`).
//!
//! This first promotion step keeps the spike's dummy-panel seed and its
//! two mechanism tests (render/focus through the wrapper, and inner-
//! layout dump/load round-trip); the VFS tree, entry tabs, cross-area
//! drag guards, and hand-composed VFS persistence land next.
#![allow(dead_code)]

use std::sync::Arc;

use gpui::App;
use gpui::AppContext;
use gpui::Context;
use gpui::Entity;
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
use gpui_component::dock::DockArea;
use gpui_component::dock::DockAreaState;
use gpui_component::dock::DockItem;
use gpui_component::dock::Panel;
use gpui_component::dock::PanelEvent;
use gpui_component::dock::PanelInfo;
use gpui_component::dock::PanelState;
use gpui_component::dock::PanelView;
use gpui_component::dock::register_panel;

/// Stable identifiers for layout (de)serialization; must never change.
pub const WORKSPACE_HOST_PANEL_NAME: &str = "WorkspaceHostPanel";
pub const DUMMY_PANEL_A_NAME: &str = "WorkspaceHostDummyA";
pub const DUMMY_PANEL_B_NAME: &str = "WorkspaceHostDummyB";

/// Register the host panel's names with gpui-component's `PanelRegistry`
/// so `DockArea::load` can rebuild a persisted layout.
pub fn register(cx: &mut App) {
    register_panel(cx, WORKSPACE_HOST_PANEL_NAME, |_dock, _state, info, window, cx| {
        Box::new(cx.new(|cx| WorkspaceHostPanel::restore(info, window, cx))) as Box<dyn PanelView>
    });
    register_panel(cx, DUMMY_PANEL_A_NAME, |_dock, _state, _info, _window, cx| {
        Box::new(cx.new(|cx| DummyPanel::new(DummyKind::A, cx))) as Box<dyn PanelView>
    });
    register_panel(cx, DUMMY_PANEL_B_NAME, |_dock, _state, _info, _window, cx| {
        Box::new(cx.new(|cx| DummyPanel::new(DummyKind::B, cx))) as Box<dyn PanelView>
    });
}

#[derive(Clone, Copy)]
enum DummyKind {
    A,
    B,
}

impl DummyKind {
    fn name(self) -> &'static str {
        match self {
            DummyKind::A => DUMMY_PANEL_A_NAME,
            DummyKind::B => DUMMY_PANEL_B_NAME,
        }
    }

    fn label(self) -> &'static str {
        match self {
            DummyKind::A => "Spike Dummy A",
            DummyKind::B => "Spike Dummy B",
        }
    }
}

/// A trivial leaf panel; two of these seed the inner dock area.
struct DummyPanel {
    focus_handle: FocusHandle,
    kind: DummyKind,
}

impl DummyPanel {
    fn new(kind: DummyKind, cx: &mut Context<Self>) -> Self {
        Self { focus_handle: cx.focus_handle(), kind }
    }
}

impl Panel for DummyPanel {
    fn panel_name(&self) -> &'static str {
        self.kind.name()
    }

    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        SharedString::from(self.kind.label())
    }
}

impl Focusable for DummyPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for DummyPanel {}

impl Render for DummyPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().track_focus(&self.focus_handle).size_full().child(self.kind.label())
    }
}

/// The spike's wrapper panel: a `Panel` whose body renders its own inner
/// `Entity<DockArea>`. gpui-component ships no such nesting example --
/// `DockArea` is a plain `Render + EventEmitter<DockEvent>` entity, not a
/// `Panel`, so nothing here is a supported combination, just a mechanically
/// possible one.
pub struct WorkspaceHostPanel {
    focus_handle: FocusHandle,
    dock: Entity<DockArea>,
}

impl WorkspaceHostPanel {
    /// Build a fresh wrapper: a new inner dock area with two dummy tabs
    /// (the spike's exercise (a) fixture).
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let dock = cx.new(|cx| new_inner_dock(window, cx));
        let weak = dock.downgrade();
        let a: Arc<dyn PanelView> = Arc::new(cx.new(|cx| DummyPanel::new(DummyKind::A, cx)));
        let b: Arc<dyn PanelView> = Arc::new(cx.new(|cx| DummyPanel::new(DummyKind::B, cx)));
        let center = DockItem::tabs(vec![a, b], &weak, window, cx);
        dock.update(cx, |dock, cx| dock.set_center(center, window, cx));
        Self { focus_handle: cx.focus_handle(), dock }
    }

    /// Rebuild from persisted `PanelInfo`. The inner `DockAreaState` is
    /// hand-serialized into this wrapper's json payload (see `dump`):
    /// `DockArea::dump`/`load` on the OUTER dock area know nothing about
    /// nesting, so persistence does not compose automatically -- the
    /// wrapper must do it itself, going back through the registry for the
    /// inner dock's own leaves.
    pub fn restore(info: &PanelInfo, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let dock = cx.new(|cx| new_inner_dock(window, cx));
        if let Some(inner_state) = inner_state_from_info(info)
            && let Err(err) = dock.update(cx, |dock, cx| dock.load(inner_state, window, cx))
        {
            tracing::warn!(%err, "spike: inner dock area load failed");
        }
        Self { focus_handle: cx.focus_handle(), dock }
    }

    pub fn inner_dock(&self) -> &Entity<DockArea> {
        &self.dock
    }
}

/// A fresh inner dock area, shared by `new` and `restore` -- both need an
/// empty one to seed (tabs) or load (a rebuilt layout) into.
fn new_inner_dock(window: &mut Window, cx: &mut Context<DockArea>) -> DockArea {
    DockArea::new("spike-inner", Some(1), window, cx)
}

fn inner_state_from_info(info: &PanelInfo) -> Option<DockAreaState> {
    let PanelInfo::Panel(value) = info else { return None };
    let raw = value.get("inner_dock_state")?;
    match serde_json::from_value(raw.clone()) {
        Ok(state) => Some(state),
        Err(err) => {
            tracing::warn!(%err, "spike: inner dock state parse failed");
            None
        }
    }
}

impl Panel for WorkspaceHostPanel {
    fn panel_name(&self) -> &'static str {
        WORKSPACE_HOST_PANEL_NAME
    }

    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        SharedString::from("Workspace Host (spike)")
    }

    /// Hand-compose the inner `DockAreaState` into this panel's json
    /// payload -- the persistence side of exercise (b).
    fn dump(&self, cx: &App) -> PanelState {
        let mut state = PanelState::new(self);
        let inner = self.dock.read(cx).dump(cx);
        let json = serde_json::to_value(&inner).expect("DockAreaState always serializes to json");
        state.info = PanelInfo::panel(serde_json::json!({ "inner_dock_state": json }));
        state
    }
}

impl Focusable for WorkspaceHostPanel {
    /// The wrapper's own handle, distinct from any inner-dock panel's
    /// handle -- clicking/focusing an inner panel focuses THAT panel's
    /// handle directly (gpui focus is a flat per-window tree; nesting a
    /// `DockArea` inside a `Panel` does not scope focus in any special
    /// way), so this handle is only reachable via `track_focus` on the
    /// wrapper's own root div, e.g. before any inner tab has been focused.
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for WorkspaceHostPanel {}

impl Render for WorkspaceHostPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().track_focus(&self.focus_handle).size_full().child(self.dock.clone())
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use gpui_component::dock::DockPlacement;

    use super::*;

    fn setup(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            register(cx);
        });
    }

    /// Flatten a `DockItem` tree to its leaf panel views, in tree order.
    fn collect_panels(item: &DockItem, out: &mut Vec<Arc<dyn PanelView>>) {
        match item {
            DockItem::Split { items, .. } => items.iter().for_each(|item| collect_panels(item, out)),
            DockItem::Tabs { items, .. } => out.extend(items.iter().cloned()),
            DockItem::Panel { view, .. } => out.push(view.clone()),
            DockItem::Tiles { .. } => {}
        }
    }

    /// Exercise (a): render a `WorkspaceHostPanel` as a window's root view
    /// (standing in for "inside a tab" -- the wrapper renders identically
    /// either way, see its `Render` impl) and confirm keyboard focus
    /// reaches a panel living inside its INNER dock area, through the
    /// wrapper.
    #[gpui::test]
    fn inner_dock_area_renders_and_focuses_through_wrapper(cx: &mut TestAppContext) {
        setup(cx);
        let window = cx.add_window(|window, cx| {
            let host = cx.new(|cx| WorkspaceHostPanel::new(window, cx));
            gpui_component::Root::new(host, window, cx)
        });
        let root = window.root(cx).unwrap();
        let host = root.read_with(cx, |root, _| root.view().clone().downcast::<WorkspaceHostPanel>().unwrap());
        let vcx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        vcx.run_until_parked();

        let panels = host.read_with(vcx, |host, cx| {
            let items = host.inner_dock().read(cx).items().clone();
            let mut out = Vec::new();
            collect_panels(&items, &mut out);
            out
        });
        assert_eq!(panels.len(), 2, "inner dock area holds both dummy panels");

        let handle_b = vcx.update(|_window, cx| {
            panels.iter().find(|p| p.panel_name(cx) == DUMMY_PANEL_B_NAME).expect("dummy B present").focus_handle(cx)
        });
        vcx.update(|window, _cx| window.focus(&handle_b));
        vcx.run_until_parked();
        assert_eq!(
            vcx.update(|window, cx| window.focused(cx)),
            Some(handle_b),
            "focus reaches an inner-dock-area panel through the wrapper"
        );
    }

    /// Exercise (b): dump a `WorkspaceHostPanel` living in an OUTER dock
    /// area, round-trip the dump through actual JSON text (not just an
    /// in-memory clone -- the persisted-layout-file path), load it into a
    /// fresh outer dock area, and confirm the registry rebuilds a real
    /// `WorkspaceHostPanel` whose inner dock area still has both dummy
    /// panels.
    #[gpui::test]
    fn dump_load_round_trips_inner_dock_state_through_registry(cx: &mut TestAppContext) {
        setup(cx);

        let window1 = cx.add_window(|window, cx| DockArea::new("spike-outer-1", None, window, cx));
        let state = window1
            .update(cx, |outer, window, cx| {
                let host = cx.new(|cx| WorkspaceHostPanel::new(window, cx));
                let view: Arc<dyn PanelView> = Arc::new(host);
                outer.add_panel(view, DockPlacement::Center, None, window, cx);
                outer.dump(cx)
            })
            .unwrap();

        let json = serde_json::to_string(&state).expect("dump serializes to json");
        let reloaded: DockAreaState = serde_json::from_str(&json).expect("json reparses to DockAreaState");

        let window2 = cx.add_window(|window, cx| DockArea::new("spike-outer-2", None, window, cx));
        window2
            .update(cx, |outer, window, cx| outer.load(reloaded, window, cx).expect("outer dock area load succeeds"))
            .unwrap();

        let inner_panel_names = window2
            .update(cx, |outer, _window, cx| {
                let mut outer_panels = Vec::new();
                collect_panels(outer.items(), &mut outer_panels);
                assert_eq!(outer_panels.len(), 1, "outer center has exactly the rebuilt host panel");
                let host = outer_panels[0]
                    .view()
                    .downcast::<WorkspaceHostPanel>()
                    .expect("registry rebuilt a real WorkspaceHostPanel");
                let mut inner_panels = Vec::new();
                collect_panels(host.read(cx).inner_dock().read(cx).items(), &mut inner_panels);
                inner_panels.iter().map(|p| p.panel_name(cx)).collect::<Vec<_>>()
            })
            .unwrap();

        assert_eq!(
            inner_panel_names,
            vec![DUMMY_PANEL_A_NAME, DUMMY_PANEL_B_NAME],
            "inner dock area's two dummy panels survive a dump -> json -> load round trip"
        );
    }
}
