//! Tab cycling, pane-pick dispatch, and the inner/outer dock
//! focus toggle.

#![cfg(not(target_arch = "wasm32"))]

use crate::app::HxyApp;
use crate::app::TabFocus;
use crate::commands::shortcuts::FOCUS_PANE;
use crate::commands::shortcuts::NEXT_TAB;
use crate::commands::shortcuts::PREV_TAB;
use crate::commands::shortcuts::TOGGLE_TAB_FOCUS;
use crate::tabs::Tab;

/// Cmd+K stages the visual pane-focus picker. No-op when a picker
/// session is already active so a double-press doesn't rebind state
/// mid-pick.
pub fn dispatch_focus_pane_shortcut(ctx: &egui::Context, app: &mut HxyApp) {
    if !ctx.input_mut(|i| i.consume_shortcut(&FOCUS_PANE)) {
        return;
    }
    if app.pending_pane_pick.is_some() {
        return;
    }
    crate::app::start_pane_focus(app);
}

/// Ctrl+Tab / Ctrl+Shift+Tab cycle tabs in the surface implied by
/// `app.tab_focus`: the outer dock's focused leaf when focus is on
/// the outer dock, or the workspace's inner dock when focus is on
/// a workspace. Wraps at the ends of the leaf's tab list. Never
/// crosses dock leaves.
pub fn dispatch_tab_cycle(ctx: &egui::Context, app: &mut HxyApp) {
    let backward = ctx.input_mut(|i| i.consume_shortcut(&PREV_TAB));
    let forward = !backward && ctx.input_mut(|i| i.consume_shortcut(&NEXT_TAB));
    if !forward && !backward {
        return;
    }
    match app.tab_focus {
        TabFocus::Outer => cycle_outer_focused_leaf(app, forward),
        TabFocus::Workspace(workspace_id) => {
            if !app.workspaces.contains_key(&workspace_id) {
                app.tab_focus = TabFocus::Outer;
                cycle_outer_focused_leaf(app, forward);
                return;
            }
            cycle_workspace_focused_leaf(app, workspace_id, forward);
        }
    }
}

fn cycle_outer_focused_leaf(app: &mut HxyApp, forward: bool) {
    let Some(node_path) = app.dock.focused_leaf() else { return };
    let Ok(leaf) = app.dock.leaf(node_path) else { return };
    let count = leaf.tabs().len();
    if count < 2 {
        return;
    }
    let current = leaf.active.0.min(count - 1);
    let next = if forward { (current + 1) % count } else { (current + count - 1) % count };
    let tab_path = egui_dock::TabPath::from((node_path, egui_dock::TabIndex(next)));
    let _ = app.dock.set_active_tab(tab_path);
}

fn cycle_workspace_focused_leaf(app: &mut HxyApp, workspace_id: crate::files::WorkspaceId, forward: bool) {
    let Some(workspace) = app.workspaces.get_mut(&workspace_id) else { return };
    let node_path = workspace.dock.focused_leaf().unwrap_or(egui_dock::NodePath {
        surface: egui_dock::SurfaceIndex::main(),
        node: egui_dock::NodeIndex::root(),
    });
    let Ok(leaf) = workspace.dock.leaf(node_path) else { return };
    let count = leaf.tabs().len();
    if count < 2 {
        return;
    }
    let current = leaf.active.0.min(count - 1);
    let next = if forward { (current + 1) % count } else { (current + count - 1) % count };
    let tab_path = egui_dock::TabPath::from((node_path, egui_dock::TabIndex(next)));
    let _ = workspace.dock.set_active_tab(tab_path);
}

/// Alt+Tab toggles `tab_focus` between the outer dock and the
/// workspace currently active in the outer dock. If the active outer
/// tab isn't a workspace, the toggle is a no-op (there's nothing to
/// switch to).
pub fn dispatch_tab_focus_toggle(ctx: &egui::Context, app: &mut HxyApp) {
    if !ctx.input_mut(|i| i.consume_shortcut(&TOGGLE_TAB_FOCUS)) {
        return;
    }
    match app.tab_focus {
        TabFocus::Outer => {
            if let Some((_, tab)) = app.dock.find_active_focused()
                && let Tab::Workspace(workspace_id) = *tab
            {
                app.tab_focus = TabFocus::Workspace(workspace_id);
            }
        }
        TabFocus::Workspace(_) => {
            app.tab_focus = TabFocus::Outer;
        }
    }
}

/// Drive one frame of the visual pane picker. Reads layout from the
/// dock (no mutation), then applies the chosen op via the same
/// helpers the directional commands use.
pub fn handle_pane_pick(ctx: &egui::Context, app: &mut HxyApp) {
    let Some(pending) = app.pending_pane_pick else { return };
    let outcome = crate::tabs::pane_pick::tick(ctx, &app.dock, pending, &mut app.pane_pick_letters);
    match outcome {
        crate::tabs::pane_pick::TickOutcome::Continue => {}
        crate::tabs::pane_pick::TickOutcome::Cancel => {
            app.pending_pane_pick = None;
        }
        crate::tabs::pane_pick::TickOutcome::Picked { source, target, op } => {
            app.pending_pane_pick = None;
            match op {
                crate::tabs::pane_pick::PaneOp::MoveTab => {
                    if let Some(source) = source {
                        crate::tabs::dock_ops::dock_move_tab_to(app, source, target);
                    }
                }
                crate::tabs::pane_pick::PaneOp::Merge => {
                    if let Some(source) = source {
                        crate::tabs::dock_ops::dock_merge_to(app, source, target);
                    }
                }
                crate::tabs::pane_pick::PaneOp::Focus => {
                    app.dock.set_focused_node_and_surface(target);
                    app.tab_focus = TabFocus::Outer;
                }
            }
        }
    }
}
