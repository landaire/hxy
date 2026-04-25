//! Visual "press a key, jump a tab" picker for dock-pane operations.
//!
//! When the user activates one of the visual move/merge palette
//! commands, the palette closes and the app enters this picker
//! mode. While active, every other dock leaf gets a big bold
//! letter painted dead-centre over its area; pressing that letter
//! executes the operation against that target. Inspired by the
//! KeyCastr-style overlay -- the goal is "obvious" not "subtle".
//!
//! All targeting is computed each frame from `DockState::iter_leaves`
//! so leaf rearrangements (resizes, drags, splits between frames)
//! don't desync the labels from the rectangles they're painted on.

#![cfg(not(target_arch = "wasm32"))]

use egui::Align2;
use egui::Color32;
use egui::FontId;
use egui::Id;
use egui::Key;
use egui::Modifiers;
use egui::Order;
use egui::Rect;
use egui::Stroke;
use egui::StrokeKind;
use egui::Vec2;
use egui_dock::NodePath;

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

/// Outcome of a single tick. The app applies it after `tick` returns
/// so the actual dock-mutating helpers can borrow `app` mutably
/// without fighting the borrow we needed to read leaf rects.
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

/// Visual constants. Tuned to read clearly over a hex view -- a big
/// rounded backdrop with a single uppercase letter centred inside.
const LABEL_BACKDROP_DIAMETER: f32 = 96.0;
const LABEL_FONT_SIZE: f32 = 64.0;
const SOURCE_FONT_SIZE: f32 = 28.0;

/// Drive one frame of the picker overlay. Call after the dock has
/// rendered (so leaf viewports are up to date for this frame) and
/// before any other handler that might consume keystrokes.
///
/// Returns the outcome the caller must act on; this fn never
/// touches the dock itself, it only reads layout and consumes input.
pub fn tick<Tab>(ctx: &egui::Context, dock: &egui_dock::DockState<Tab>, pending: PendingPanePick) -> TickOutcome
where
    Tab: Clone,
{
    // Cancel immediately if Escape is pressed; even if there are no
    // targets we want this to still close cleanly.
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        return TickOutcome::Cancel;
    }

    // Targets = every leaf except the source (when there is one),
    // in stable iteration order so labels don't shuffle between
    // frames. Sourceless ops (Focus) include every leaf so the user
    // can also "stay where I am" by picking the current one.
    let mut targets: Vec<(NodePath, Rect)> = dock
        .iter_leaves()
        .filter(|(p, _)| pending.source.is_none_or(|s| *p != s))
        .map(|(p, l)| (p, l.rect))
        .filter(|(_, r)| r.is_finite() && r.width() > 1.0 && r.height() > 1.0)
        .collect();
    // Stable label assignment: sort by (surface, node) so adding /
    // removing leaves elsewhere doesn't relabel the survivors.
    targets.sort_by_key(|(p, _)| (p.surface.0, p.node.0));
    if targets.is_empty() {
        return TickOutcome::Cancel;
    }
    // Cap at 26 (a..z). Beyond that we'd need two-char labels which
    // defeats the "press one key" UX; just hide the overflow leaves
    // and let the user split fewer panes.
    targets.truncate(26);

    // Find the source leaf's rect for the "FROM" badge. iter_leaves
    // is the source of truth for layout this frame. Sourceless ops
    // (Focus) skip the badge entirely.
    let source_rect = pending
        .source
        .and_then(|src| dock.iter_leaves().find(|(p, _)| *p == src).map(|(_, l)| l.rect));

    // Look up which letter (if any) was pressed this frame. Walk
    // a..z so the first match wins -- only one letter can be live
    // anyway since each label is unique.
    let pressed_letter = ctx.input_mut(|i| {
        for (idx, _) in targets.iter().enumerate() {
            let key = letter_key(idx);
            if i.consume_key(Modifiers::NONE, key) {
                return Some(idx);
            }
        }
        None
    });

    // Paint the overlays after consuming input so a click-anywhere
    // doesn't accidentally swallow a target letter the user just
    // pressed. Full-screen transparent backdrop swallows cursor
    // events so the dock underneath doesn't react to a click during
    // the pick.
    let visuals = ctx.global_style().visuals.clone();
    let backdrop_fill = if visuals.dark_mode {
        Color32::from_rgba_unmultiplied(0, 0, 0, 90)
    } else {
        Color32::from_rgba_unmultiplied(0, 0, 0, 60)
    };
    egui::Area::new(Id::new("hxy-pane-pick-backdrop"))
        .order(Order::Foreground)
        .fixed_pos(ctx.content_rect().min)
        .interactable(true)
        .show(ctx, |ui| {
            let (rect, _resp) = ui.allocate_exact_size(ctx.content_rect().size(), egui::Sense::click());
            ui.painter().rect_filled(rect, 0.0, backdrop_fill);
        });

    if let Some(rect) = source_rect {
        paint_source_badge(ctx, rect, pending.op, &visuals);
    }
    for (idx, (_, rect)) in targets.iter().enumerate() {
        paint_target_label(ctx, *rect, letter_for(idx), &visuals);
    }

    if let Some(idx) = pressed_letter {
        let (target, _) = targets[idx];
        return TickOutcome::Picked { source: pending.source, target, op: pending.op };
    }

    // Picker stays active across frames; repaint so a subsequent
    // resize updates the overlay positions immediately.
    ctx.request_repaint();
    TickOutcome::Continue
}

fn letter_for(idx: usize) -> char {
    debug_assert!(idx < 26, "letter_for called past z; targets must be capped first");
    (b'A' + idx as u8) as char
}

fn letter_key(idx: usize) -> Key {
    use Key::*;
    [A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z][idx.min(25)]
}

fn paint_target_label(ctx: &egui::Context, leaf_rect: Rect, letter: char, visuals: &egui::Visuals) {
    let centre = leaf_rect.center();
    let backdrop = Rect::from_center_size(centre, Vec2::splat(LABEL_BACKDROP_DIAMETER));
    let id = Id::new(("hxy-pane-pick-target", letter));
    egui::Area::new(id)
        .order(Order::Foreground)
        .fixed_pos(backdrop.min)
        .interactable(false)
        .show(ctx, |ui| {
            let painter = ui.painter();
            let fill = visuals.selection.bg_fill;
            let stroke = Stroke::new(2.0, visuals.strong_text_color());
            painter.rect_filled(backdrop, 16.0, fill);
            painter.rect_stroke(backdrop, 16.0, stroke, StrokeKind::Inside);
            painter.text(
                backdrop.center(),
                Align2::CENTER_CENTER,
                letter,
                FontId::monospace(LABEL_FONT_SIZE),
                visuals.strong_text_color(),
            );
        });
}

fn paint_source_badge(ctx: &egui::Context, leaf_rect: Rect, op: PaneOp, visuals: &egui::Visuals) {
    let label = match op {
        PaneOp::MoveTab => "MOVE FROM",
        PaneOp::Merge => "MERGE FROM",
        // Focus is sourceless; the caller doesn't paint a badge for
        // it. Match arm exists for exhaustiveness only.
        PaneOp::Focus => return,
    };
    let centre = leaf_rect.center();
    let id = Id::new("hxy-pane-pick-source");
    egui::Area::new(id)
        .order(Order::Foreground)
        .fixed_pos(centre)
        .interactable(false)
        .show(ctx, |ui| {
            let painter = ui.painter();
            let font = FontId::monospace(SOURCE_FONT_SIZE);
            let galley = ui.fonts_mut(|f| {
                f.layout_no_wrap(label.to_owned(), font.clone(), visuals.weak_text_color())
            });
            let pad = Vec2::new(20.0, 12.0);
            let backdrop = Rect::from_center_size(centre, galley.size() + pad * 2.0);
            painter.rect_filled(backdrop, 12.0, visuals.extreme_bg_color);
            painter.rect_stroke(
                backdrop,
                12.0,
                Stroke::new(1.5, visuals.weak_text_color()),
                StrokeKind::Inside,
            );
            painter.text(
                backdrop.center(),
                Align2::CENTER_CENTER,
                label,
                font,
                visuals.text_color(),
            );
        });
}
