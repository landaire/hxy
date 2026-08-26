//! Vimium-style keyboard pane picker for [`gpui::component::dock::DockArea`]
//! docks: activate, press a letter, focus jumps there.
//!
//! [`DockPicker`] is a reusable `gpui` view a host mounts once (like a
//! second child alongside the dock) and drives via [`DockPicker::activate`].
//! While a pick session is active it grabs keyboard focus, renders each
//! target's letter as a badge over its pane (the vimium-style visual
//! picker; a target with no on-screen rect falls back to a listed row),
//! and resolves the session on the next keystroke: a matching letter picks
//! that target and invokes the host's callback, `Escape` or a backdrop
//! click cancels and restores whichever element had focus before
//! activation, and any other key is ignored (the session stays open).
//!
//! [`PickTarget`] is a plain `{ label, focus, on_activate, bounds }` shape,
//! not tied to `DockArea` at all -- [`DockPicker::activate`] takes a
//! host-supplied `Vec<PickTarget>` directly. [`PickTarget::from_dock_area`]
//! is a convenience that enumerates one `DockArea`'s pickable center leaves
//! (badging each via `DockArea::node_bounds`); a host composes its own list
//! from that plus whatever else it wants picked (a side-dock panel it
//! tracks itself, a target in a second `DockArea`, ...).
//!
//! # gpui-component API note
//!
//! **Only center-dock leaves are enumerated.** `DockArea`'s `left_dock` /
//! `right_dock` / `bottom_dock` fields are private with no enumeration
//! accessor (only `has_dock`/`is_dock_open` booleans), so side-dock panels
//! cannot be discovered from outside gpui-component. A host that tracks a
//! side-dock panel's entity itself (e.g. by self-publishing it to a global
//! at construction) can still make it pickable: build a `PickTarget::new`
//! by hand and push it onto `from_dock_area`'s list -- it renders as a
//! listed row since it carries no pane rect.
//!
//! `from_dock_area`'s targets are one per live `TabPanel` leaf in the
//! center tree (i.e. one entry per `DockItem::Tabs`, found by walking
//! `DockItem::Split` recursively), labeled with the leaf's active tab's
//! name. `DockPicker::activate` assigns `a`..`z` in the order targets are
//! given; beyond 26 the overflow is silently dropped (mirrors
//! `egui_dock_picker`). Letter assignment is NOT sticky across sessions
//! (unlike `egui_dock_picker`'s per-leaf `BTreeMap<u64, char>`, which keeps
//! a leaf's letter stable across pick sessions even as others open/close):
//! every `activate` call re-assigns from scratch in target order. Recorded
//! as a deliberate scope cut for M2, not an oversight -- sticky assignment
//! needs a per-leaf stable identity to key off of, which is its own design
//! question or a straightforward M3 follow-up.

use std::rc::Rc;

use gpui::App;
use gpui::Bounds;
use gpui::Context;
use gpui::Entity;
use gpui::FocusHandle;
use gpui::Focusable;
use gpui::InteractiveElement;
use gpui::IntoElement;
use gpui::KeyDownEvent;
use gpui::MouseButton;
use gpui::ParentElement;
use gpui::Pixels;
use gpui::Render;
use gpui::SharedString;
use gpui::Styled;
use gpui::Window;
use gpui::div;
use gpui::px;
use gpui::component::ActiveTheme;
use gpui::component::dock::DockArea;
use gpui::component::dock::DockPlacement;
use gpui::component::dock::PaneRef;
use gpui::component::dock::PanelHandle;
use gpui::component::h_flex;
use gpui::component::v_flex;

/// Activation behavior a target can override; see [`PickTarget::with_on_activate`].
type OnActivate = Rc<dyn Fn(&mut Window, &mut App)>;

/// A pickable jump target: a display label and what picking it does.
/// Deliberately NOT tied to `DockArea`/`TabPanel` -- construct one with
/// [`PickTarget::new`] for anything with a [`FocusHandle`] (a host's own
/// side-dock panel, a target from a second `DockArea`, ...), or get a
/// batch of them for one `DockArea`'s center leaves via
/// [`PickTarget::from_dock_area`].
#[derive(Clone)]
pub struct PickTarget {
    label: SharedString,
    focus: FocusHandle,
    on_activate: Option<OnActivate>,
    /// The target's on-screen rectangle, when known. Present for dock
    /// leaves (via [`Self::from_dock_area`], which reads
    /// `DockArea::node_bounds`); absent for host-supplied targets that
    /// occupy no pane. When present the overlay paints the target's letter
    /// as a badge centered on this rect (the vimium-style visual picker);
    /// boundsless targets fall back to a listed row.
    bounds: Option<Bounds<Pixels>>,
}

impl PickTarget {
    /// A target that, by default, just moves keyboard focus to `focus`
    /// when picked. Use [`Self::with_on_activate`] to override that (e.g.
    /// to open a collapsed dock first).
    pub fn new(label: impl Into<SharedString>, focus: FocusHandle) -> Self {
        Self { label: label.into(), focus, on_activate: None, bounds: None }
    }

    /// Give this target an on-screen rectangle so the overlay badges its
    /// letter over that pane instead of listing it.
    pub fn with_bounds(mut self, bounds: Bounds<Pixels>) -> Self {
        self.bounds = Some(bounds);
        self
    }

    /// This target's on-screen rectangle, if it occupies one.
    pub fn bounds(&self) -> Option<Bounds<Pixels>> {
        self.bounds
    }

    /// Override this target's activation: instead of a plain focus move,
    /// `on_activate` runs when it's picked. Typical use: a target whose
    /// content needs to be made visible first (e.g. opening a collapsed
    /// dock) before focusing it -- `on_activate` is responsible for the
    /// focus move too in that case, `DockPicker` will not also focus
    /// `focus` afterward.
    pub fn with_on_activate(mut self, on_activate: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_activate = Some(Rc::new(on_activate));
        self
    }

    /// This target's display label.
    pub fn label(&self) -> &SharedString {
        &self.label
    }

    /// This target's focus handle -- what a plain (no `on_activate`) pick
    /// focuses.
    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// Run this target's activation: `on_activate` if set, else a plain
    /// focus move to `focus`.
    fn activate(&self, window: &mut Window, cx: &mut App) {
        match &self.on_activate {
            Some(on_activate) => on_activate(window, cx),
            None => window.focus(&self.focus, cx),
        }
    }

    /// One target per live `TabPanel` leaf in `dock_area`'s center tree,
    /// in tree-walk order, labeled with the leaf's active tab's name --
    /// or `empty_label` when the leaf has no active panel or the panel has
    /// no tab name. `empty_label` is host-supplied rather than a built-in
    /// English default: this crate has no i18n dependency by design, and a
    /// hardcoded fallback string would silently render unlocalized text in
    /// a host that otherwise localizes everything.
    ///
    /// See the crate docs for why side docks aren't included here.
    pub fn from_dock_area(
        dock_area: &Entity<DockArea>,
        empty_label: impl Into<SharedString>,
        cx: &App,
    ) -> Vec<PickTarget> {
        let empty_label = empty_label.into();
        let area = dock_area.read(cx);
        let mut targets = Vec::new();
        let Some(tree) = area.layout(DockPlacement::Center) else { return targets };
        tree.root().walk(&mut |node| {
            let PaneRef::Tabs { panels, active_ix } = node.kind() else { return };
            let Some(panel_id) = panels.get(active_ix).copied() else { return };
            let Some(panel) = area.panel(panel_id) else { return };
            let label = PanelHandle::of(panel)
                .and_then(|handle| handle.tab_name(cx))
                .unwrap_or_else(|| empty_label.clone());
            // The panel's own focus handle reaches the leaf's real
            // content (e.g. a hosted hex pane), matching the old
            // `TabPanel::focus_handle` delegation.
            let mut target = PickTarget::new(label, panel.focus_handle(cx));
            // Carry the leaf's rect so the overlay badges its letter over
            // the pane (the visual picker). Absent on the frame a leaf is
            // first created, before its first prepaint; the overlay then
            // lists that one target until the next frame.
            if let Some(bounds) = area.node_bounds(node.id()) {
                target = target.with_bounds(bounds);
            }
            targets.push(target);
        });
        targets
    }
}

/// Callback invoked with the picked target when a session resolves.
type OnPick = Rc<dyn Fn(PickTarget, &mut Window, &mut App)>;

/// One active pick session: the assigned letters, the callback to invoke
/// on a hit, and the focus to restore on cancel.
struct Session {
    targets: Vec<(char, PickTarget)>,
    restore_focus: Option<FocusHandle>,
    on_pick: OnPick,
}

/// The picker overlay view. Mount one per window (a host typically owns it
/// alongside its `DockArea`, e.g. as a sibling child in its root render).
/// Renders nothing while inactive.
pub struct DockPicker {
    focus_handle: FocusHandle,
    active: Option<Session>,
}

impl DockPicker {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self { focus_handle: cx.focus_handle(), active: None }
    }

    /// Whether a pick session is currently active.
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// The current session's letter-assigned targets, in tree-walk order.
    /// Empty when no session is active. Exposed so a host can render its
    /// own overlay (e.g. a localized one) instead of this crate's default,
    /// or introspect state in tests.
    pub fn targets(&self) -> &[(char, PickTarget)] {
        self.active.as_ref().map_or(&[], |session| session.targets.as_slice())
    }

    /// Assign each of `targets` the next free letter (in the order given),
    /// stash the currently focused element for restore-on-cancel, and grab
    /// keyboard focus so the next keystroke resolves the pick. No-op (does
    /// not open a session or move focus) when `targets` is empty.
    ///
    /// Safe to call while a session is already active (hosts are not
    /// required to guard against this themselves, though `is_active`
    /// lets them skip the rescan if they want to): the new session's
    /// targets replace the old ones, but the restore-on-cancel focus
    /// carries forward from the original session rather than being
    /// re-captured -- by the time a second `activate` call lands,
    /// `window.focused(cx)` is the picker's own overlay, and stashing
    /// that would permanently lose the real pre-activation focus target
    /// (mirrors `Palette::toggle`'s identical rule, for the identical
    /// reason).
    pub fn activate(
        &mut self,
        targets: Vec<PickTarget>,
        on_pick: impl Fn(PickTarget, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if targets.is_empty() {
            return;
        }
        let targets: Vec<(char, PickTarget)> = ('a'..='z').zip(targets).collect();
        let restore_focus = match self.active.take() {
            Some(session) => session.restore_focus,
            None => window.focused(cx),
        };
        self.active = Some(Session { targets, restore_focus, on_pick: Rc::new(on_pick) });
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// End the current session without picking, restoring whichever
    /// element had focus before [`Self::activate`]. No-op when inactive.
    pub fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.active.take() else { return };
        if let Some(handle) = session.restore_focus {
            window.focus(&handle, cx);
        }
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.active else { return };
        let keystroke = &event.keystroke;
        if !keystroke.modifiers.modified() && keystroke.key == "escape" {
            cx.stop_propagation();
            self.cancel(window, cx);
            return;
        }
        if keystroke.modifiers.modified() {
            return;
        }
        let Some((_, target)) =
            session.targets.iter().find(|(letter, _)| keystroke.key.len() == 1 && keystroke.key.starts_with(*letter))
        else {
            // Anything else (not Escape, not an assigned letter) is
            // ignored: the session stays open for another try, mirroring
            // `egui_dock_picker::tick`'s choice not to treat a stray
            // keystroke as a cancel.
            return;
        };
        let target = target.clone();
        let on_pick = session.on_pick.clone();
        cx.stop_propagation();
        self.active = None;
        cx.notify();
        target.activate(window, cx);
        on_pick(target, window, cx);
    }
}

/// Collect every `TabPanel` leaf under `item`, in tree order.
impl Focusable for DockPicker {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DockPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(session) = &self.active else {
            return div().into_any_element();
        };

        let (base, muted, popover, border, accent, accent_fg) = {
            let theme = cx.theme();
            (theme.foreground, theme.muted_foreground, theme.popover, theme.border, theme.accent, theme.accent_foreground)
        };

        // A target with a rect is badged over its pane (the visual
        // picker); one without (a host-supplied off-pane target, e.g. the
        // side-dock inspector) falls back to a listed row.
        let badges: Vec<gpui::AnyElement> = session
            .targets
            .iter()
            .filter_map(|(letter, target)| target.bounds().map(|bounds| (letter, bounds)))
            .map(|(letter, bounds)| {
                let size = px(96.0);
                let center_x = bounds.origin.x + bounds.size.width * 0.5;
                let center_y = bounds.origin.y + bounds.size.height * 0.5;
                div()
                    .absolute()
                    .left(center_x - size * 0.5)
                    .top(center_y - size * 0.5)
                    .w(size)
                    .h(size)
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_2xl()
                    .bg(accent)
                    .border_2()
                    .border_color(accent_fg)
                    .shadow_lg()
                    .text_color(accent_fg)
                    .text_size(px(64.0))
                    .child(letter.to_ascii_uppercase().to_string())
                    .into_any_element()
            })
            .collect();

        let rows: Vec<gpui::AnyElement> = session
            .targets
            .iter()
            .filter(|(_, target)| target.bounds().is_none())
            .map(|(letter, target)| {
                let label = target.label().clone();
                h_flex()
                    .gap_2()
                    .items_center()
                    .px_2()
                    .py_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(24.0))
                            .h(px(24.0))
                            .rounded_md()
                            .bg(muted.opacity(0.2))
                            .text_color(base)
                            .child(letter.to_ascii_uppercase().to_string()),
                    )
                    .child(div().text_color(base).child(label))
                    .into_any_element()
            })
            .collect();

        let backdrop = div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .bg(gpui::black().opacity(0.35))
            .occlude()
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _ev, window, cx| this.cancel(window, cx)));

        // The boundsless targets' list, anchored bottom-center so it does
        // not sit on top of the center pane's badge. Absent when every
        // target is badged.
        let list = (!rows.is_empty()).then(|| {
            div()
                .absolute()
                .bottom(px(24.0))
                .child(
                    v_flex()
                        .w(px(280.0))
                        .bg(popover)
                        .border_1()
                        .border_color(border)
                        .rounded_lg()
                        .shadow_lg()
                        .p_1()
                        .gap_1()
                        .occlude()
                        .children(rows),
                )
        });

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .child(backdrop)
            .children(badges)
            .children(list)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::Arc;

    use gpui::AppContext as _;
    use gpui::EventEmitter;
    use gpui::SharedString;
    use gpui::TestAppContext;
    use gpui::VisualTestContext;
    use gpui::WindowHandle;
    use gpui::component::dock::BasePanel;
    use gpui::component::dock::DockLayout;
    use gpui::component::dock::Panel;
    use gpui::component::dock::BasePanelView;
    use gpui::component::dock::PanelEvent;
    use gpui::component::dock::PanelHandle;

    use super::*;

    /// A minimal `Panel` for tests: just a name and a focus handle, no
    /// real content.
    struct TestPanel {
        name: SharedString,
        focus_handle: FocusHandle,
    }

    impl TestPanel {
        fn new(name: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
            Self { name: name.into(), focus_handle: cx.focus_handle() }
        }
    }

    impl BasePanel for TestPanel {
        fn panel_name(&self) -> &'static str {
            "TestPanel"
        }
    }

    impl Panel for TestPanel {
        fn tab_name(&self, _cx: &App) -> Option<SharedString> {
            Some(self.name.clone())
        }
    }

    impl Focusable for TestPanel {
        fn focus_handle(&self, _cx: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl EventEmitter<PanelEvent> for TestPanel {}

    impl Render for TestPanel {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().track_focus(&self.focus_handle).size_full()
        }
    }

    struct Host {
        dock_area: Entity<DockArea>,
        picker: Entity<DockPicker>,
    }

    impl Render for Host {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.dock_area.clone()).child(self.picker.clone())
        }
    }

    /// Builds a window whose root hosts a `DockArea` with `panel_count`
    /// separate center-dock leaves (one `TestPanel` tab each, named
    /// "Panel 0", "Panel 1", ...) plus a `DockPicker`, and returns the
    /// dock area / picker / per-panel focus handles for driving tests.
    fn build(
        cx: &mut TestAppContext,
        panel_count: usize,
    ) -> (Entity<DockArea>, Entity<DockPicker>, Vec<FocusHandle>, &mut VisualTestContext) {
        cx.update(gpui::component::init);
        let mut panel_handles = Vec::new();
        let window: WindowHandle<Host> = cx.add_window(|window, cx| {
            let dock_area = cx.new(|cx| DockArea::new("test", None, window, cx));
            let mut handles = Vec::with_capacity(panel_count);
            // One horizontal split with `panel_count` single-tab leaves side
            // by side, so the picker sees distinct left-to-right panes.
            let mut center = DockLayout::h_split();
            for i in 0..panel_count {
                let panel = cx.new(|cx| TestPanel::new(format!("Panel {i}"), cx));
                handles.push(panel.read(cx).focus_handle(cx));
                let view: Arc<dyn BasePanelView> = Arc::new(PanelHandle::new(panel));
                center = center.child(DockLayout::tabs().panel_view(view, cx), None);
            }
            dock_area.update(cx, |dock, cx| dock.set_center(center, window, cx));
            panel_handles = handles;
            let picker = cx.new(DockPicker::new);
            Host { dock_area, picker }
        });
        let root = window.root(cx).unwrap();
        let dock_area = root.read_with(cx, |host, _| host.dock_area.clone());
        let picker = root.read_with(cx, |host, _| host.picker.clone());
        let vcx = VisualTestContext::from_window(*window, cx).into_mut();
        vcx.run_until_parked();
        (dock_area, picker, panel_handles, vcx)
    }

    /// `PickTarget::from_dock_area` with the crate-agnostic empty-leaf
    /// label used throughout these tests.
    fn dock_targets(dock_area: &Entity<DockArea>, cx: &App) -> Vec<PickTarget> {
        PickTarget::from_dock_area(dock_area, "(empty)", cx)
    }


    /// Activating with a 3-leaf dock plus one host-injected target (the
    /// composition pattern `from_dock_area`'s doc describes: a host's own
    /// extras concatenated onto the dock-derived list) enumerates all four
    /// as distinct letters, in the order given -- dock leaves first, the
    /// injected target last.
    #[gpui::test]
    fn activate_enumerates_targets_with_distinct_letters(cx: &mut TestAppContext) {
        let (dock_area, picker, handles, cx) = build(cx, 3);

        cx.update(|window, cx| {
            let mut targets = dock_targets(&dock_area, cx);
            targets.push(PickTarget::new("Injected", handles[0].clone()));
            picker.update(cx, |picker, cx| picker.activate(targets, |_, _, _| {}, window, cx));
        });

        let letters: Vec<char> = picker.read_with(cx, |picker, _| picker.targets().iter().map(|(l, _)| *l).collect());
        assert_eq!(letters, vec!['a', 'b', 'c', 'd'], "the injected target gets the next letter after the dock's own");
        assert!(picker.read_with(cx, |picker, _| picker.is_active()));

        let labels: Vec<String> =
            picker.read_with(cx, |picker, _| picker.targets().iter().map(|(_, t)| t.label().to_string()).collect());
        assert_eq!(labels, vec!["Panel 0", "Panel 1", "Panel 2", "Injected"]);
    }

    /// Pressing an assigned letter resolves the pick: the callback fires
    /// with the right target, and -- since this target has no
    /// `on_activate` override -- the crate's default plain-focus behavior
    /// moves focus to that leaf's panel.
    #[gpui::test]
    fn letter_keystroke_picks_the_target_and_moves_focus(cx: &mut TestAppContext) {
        let (dock_area, picker, handles, cx) = build(cx, 3);

        cx.update(|window, cx| {
            // Start focus on panel 0 so the eventual focus change is
            // observable (not merely "already there").
            window.focus(&handles[0], cx);
        });

        let picked: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let picked_for_cb = picked.clone();
        cx.update(|window, cx| {
            let targets = dock_targets(&dock_area, cx);
            picker.update(cx, |picker, cx| {
                picker.activate(
                    targets,
                    move |target, _window, _cx| {
                        *picked_for_cb.borrow_mut() = Some(target.label().to_string());
                    },
                    window,
                    cx,
                );
            });
        });
        assert!(picker.read_with(cx, |picker, _| picker.is_active()), "activate opens a session");

        cx.simulate_keystrokes("b");

        assert_eq!(picked.borrow().as_deref(), Some("Panel 1"), "letter b picks the second leaf (0-indexed panel 1)");
        assert!(!picker.read_with(cx, |picker, _| picker.is_active()), "a successful pick closes the session");
        assert_eq!(
            cx.update(|window, cx| window.focused(cx)),
            Some(handles[1].clone()),
            "focus moved to the picked panel"
        );
    }

    /// A target's `on_activate` override runs INSTEAD of the crate's
    /// default plain focus, and is fully responsible for the focus move --
    /// proven by pointing it at a DIFFERENT handle than the target's own
    /// and confirming that's what ends up focused, not the target's.
    #[gpui::test]
    fn on_activate_override_runs_instead_of_plain_focus(cx: &mut TestAppContext) {
        let (dock_area, picker, handles, cx) = build(cx, 2);

        let custom_ran = Rc::new(RefCell::new(false));
        let custom_ran_for_cb = custom_ran.clone();
        let other_handle = handles[1].clone();
        cx.update(|window, cx| {
            let mut targets = dock_targets(&dock_area, cx);
            // Override the first (letter 'a') target -- panel 0's own
            // leaf -- to focus panel 1's handle instead of its own.
            targets[0] = targets[0].clone().with_on_activate(move |window, cx| {
                *custom_ran_for_cb.borrow_mut() = true;
                window.focus(&other_handle, cx);
            });
            picker.update(cx, |picker, cx| picker.activate(targets, |_, _, _| {}, window, cx));
        });

        cx.simulate_keystrokes("a");

        assert!(*custom_ran.borrow(), "on_activate override must run on pick");
        assert_eq!(
            cx.update(|window, cx| window.focused(cx)),
            Some(handles[1].clone()),
            "on_activate is fully responsible for the focus move when set -- the default plain focus must not also run"
        );
    }

    /// Escape cancels without picking and restores the focus that was
    /// active before the session opened -- no focus leak.
    #[gpui::test]
    fn escape_cancels_and_restores_prior_focus(cx: &mut TestAppContext) {
        let (dock_area, picker, handles, cx) = build(cx, 3);

        cx.update(|window, cx| window.focus(&handles[2], cx));
        cx.update(|window, cx| {
            let targets = dock_targets(&dock_area, cx);
            picker.update(cx, |picker, cx| picker.activate(targets, |_, _, _| {}, window, cx));
        });
        assert!(picker.read_with(cx, |picker, _| picker.is_active()));

        cx.simulate_keystrokes("escape");

        assert!(!picker.read_with(cx, |picker, _| picker.is_active()), "escape closes the session");
        assert_eq!(
            cx.update(|window, cx| window.focused(cx)),
            Some(handles[2].clone()),
            "escape restores the pre-activation focus"
        );
    }

    /// Calling `activate` a second time while already active (a host bug,
    /// but one the crate should tolerate) must not clobber the restore
    /// focus with the picker's own overlay handle -- cancelling after the
    /// re-activation still needs to land back on the true pre-activation
    /// element.
    #[gpui::test]
    fn reactivating_mid_session_preserves_the_original_restore_focus(cx: &mut TestAppContext) {
        let (dock_area, picker, handles, cx) = build(cx, 3);

        cx.update(|window, cx| window.focus(&handles[0], cx));
        cx.update(|window, cx| {
            let targets = dock_targets(&dock_area, cx);
            picker.update(cx, |picker, cx| picker.activate(targets, |_, _, _| {}, window, cx));
        });
        assert!(picker.read_with(cx, |picker, _| picker.is_active()));

        // Re-activate while already active. `window.focused` is now the
        // picker's own handle; a naive re-stash would capture that.
        cx.update(|window, cx| {
            let targets = dock_targets(&dock_area, cx);
            picker.update(cx, |picker, cx| picker.activate(targets, |_, _, _| {}, window, cx));
        });
        assert!(picker.read_with(cx, |picker, _| picker.is_active()), "still active after re-activation");

        cx.simulate_keystrokes("escape");

        assert!(!picker.read_with(cx, |picker, _| picker.is_active()));
        assert_eq!(
            cx.update(|window, cx| window.focused(cx)),
            Some(handles[0].clone()),
            "cancel after a re-activation must restore the ORIGINAL pre-activation focus, not the picker's own"
        );
    }

    /// An unassigned key (not Escape, not a live letter) is ignored: the
    /// session stays open for another try, mirroring `egui_dock_picker`.
    #[gpui::test]
    fn unmatched_key_is_ignored_and_session_stays_open(cx: &mut TestAppContext) {
        let (dock_area, picker, _handles, cx) = build(cx, 2);

        cx.update(|window, cx| {
            let targets = dock_targets(&dock_area, cx);
            picker.update(cx, |picker, cx| picker.activate(targets, |_, _, _| {}, window, cx));
        });

        // 'z' is not assigned (only 'a'/'b' are live for a 2-leaf dock).
        cx.simulate_keystrokes("z");

        assert!(picker.read_with(cx, |picker, _| picker.is_active()), "an unassigned key does not cancel the session");
    }

    /// A dock with no center leaves yields nothing to pick: `activate` is
    /// a no-op (no session opens, focus does not move).
    #[gpui::test]
    fn activate_on_an_empty_dock_is_a_noop(cx: &mut TestAppContext) {
        let (dock_area, picker, _handles, cx) = build(cx, 0);

        let focused_before = cx.update(|window, cx| window.focused(cx));
        cx.update(|window, cx| {
            let targets = dock_targets(&dock_area, cx);
            picker.update(cx, |picker, cx| picker.activate(targets, |_, _, _| {}, window, cx));
        });

        assert!(!picker.read_with(cx, |picker, _| picker.is_active()), "no leaves means no session");
        assert_eq!(cx.update(|window, cx| window.focused(cx)), focused_before, "focus is untouched");
    }
}
