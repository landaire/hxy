//! Hxy adapter around the generic `egui_dock_picker` crate.
//!
//! The generic picker doesn't know what op is being staged -- it just
//! resolves a target leaf when the user presses a letter. This file
//! wraps it with hxy's [`PaneOp`] enum so the rest of the app can
//! dispatch on op without leaking that vocabulary into the picker.

use std::collections::BTreeMap;
use std::hash::Hash;

use egui_dock::NodePath;
pub use egui_dock_picker::PanePickConfig;

/// Which dock operation the picker is staging. Captured at the
/// moment the user picks the palette command so the executor knows
/// what to do once they hit a target letter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneOp {
    /// Move just the source leaf's currently-active tab into the
    /// target leaf. Sibling tabs stay put.
    MoveTab,
    /// Move every tab from the source leaf into the target and
    /// remove the now-empty source so the parent split collapses.
    Merge,
    /// Move keyboard focus to the picked leaf. Sourceless: every
    /// leaf in the dock is a target, including the currently
    /// focused one (a no-op pick).
    Focus,
    /// Close every tool-class tab in the picked leaf. Sourceless;
    /// the host pre-filters the candidate target list to leaves
    /// that are entirely tool tabs so the picker letters only
    /// appear on closeable panes.
    CloseToolLeaf,
}

/// State of a picker session. The app stores `Option<PendingPanePick>`
/// and clears it once a target letter is pressed (operation runs)
/// or Escape is pressed (cancelled). `source` is `None` for
/// sourceless ops like `Focus`.
#[derive(Clone, Copy, Debug)]
pub struct PendingPanePick {
    pub op: PaneOp,
    pub source: Option<NodePath>,
}

/// Outcome of a single tick. Same shape as
/// [`egui_dock_picker::TickOutcome`] plus the staged [`PaneOp`] so
/// the host can dispatch without a side lookup.
pub enum TickOutcome {
    /// Picker still running; keep state, repaint next frame.
    Continue,
    /// User cancelled (Escape, no targets, or invalid source). Clear
    /// the picker state.
    Cancel,
    /// User pressed a target letter. Execute the staged op against
    /// `target` and clear the picker state. `source` is `None` for
    /// sourceless ops like `Focus`.
    Picked { source: Option<NodePath>, target: NodePath, op: PaneOp },
}

pub fn tick<Tab>(
    ctx: &egui::Context,
    dock: &egui_dock::DockState<Tab>,
    pending: PendingPanePick,
    assignments: &mut BTreeMap<u64, char>,
    target_whitelist: Option<&[NodePath]>,
) -> TickOutcome
where
    Tab: Clone + Hash,
{
    let badge = match pending.op {
        PaneOp::MoveTab => Some("MOVE FROM"),
        PaneOp::Merge => Some("MERGE FROM"),
        // Focus and CloseToolLeaf are sourceless; the source rect
        // (if any) shouldn't be flagged.
        PaneOp::Focus | PaneOp::CloseToolLeaf => None,
    };
    let config =
        egui_dock_picker::PanePickConfig { source: pending.source, source_badge_label: badge, target_whitelist };
    match egui_dock_picker::tick(ctx, dock, config, assignments) {
        egui_dock_picker::TickOutcome::Continue => TickOutcome::Continue,
        egui_dock_picker::TickOutcome::Cancel => TickOutcome::Cancel,
        egui_dock_picker::TickOutcome::Picked { source, target } => {
            TickOutcome::Picked { source, target, op: pending.op }
        }
    }
}
