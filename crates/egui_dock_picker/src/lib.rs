//! Visual "press a key, jump a pane" picker overlay for `egui_dock`.
//!
//! See the crate-level README for a quick example. The headline
//! types are [`tick`] (the per-frame entry point), [`PanePickConfig`]
//! (what the host stages), and [`TickOutcome`] (what the host acts
//! on).
//!
//! All targeting is computed each frame from
//! [`DockState::iter_leaves`](egui_dock::DockState::iter_leaves) so
//! leaf rearrangements (resizes, drags, splits between frames) don't
//! desync the labels from the rectangles they're painted on.

#![warn(missing_docs)]

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::hash::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;

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

/// Visual constants. Tuned to read clearly over a busy view -- a big
/// rounded backdrop with a single uppercase letter centered inside.
const LABEL_BACKDROP_DIAMETER: f32 = 96.0;
const LABEL_FONT_SIZE: f32 = 64.0;
const SOURCE_FONT_SIZE: f32 = 28.0;

/// What the host stages when entering a pick. The picker has no
/// opinion on what `source` means or what to do once `target` is
/// resolved -- that's the host's responsibility.
#[derive(Clone, Copy, Debug)]
pub struct PanePickConfig<'a> {
    /// The "from" leaf, if any. When set, this leaf is excluded from
    /// the candidate target set and gets a badge painted over it.
    /// `None` for sourceless picks (e.g. "focus a pane", "close a
    /// pane").
    pub source: Option<NodePath>,

    /// Optional badge label drawn over the source leaf. Ignored when
    /// `source` is `None`. Typical values: "MOVE FROM", "MERGE FROM".
    pub source_badge_label: Option<&'a str>,

    /// If `Some`, restrict the candidate target set to this list. Use
    /// for ops that only apply to a subset of leaves (e.g. "close
    /// tool pane" only targets leaves whose tabs are all tool tabs).
    pub target_whitelist: Option<&'a [NodePath]>,
}

/// Outcome of a single [`tick`] call. The host applies it after the
/// call returns so the actual dock-mutating helpers can borrow the
/// host mutably without fighting the borrow we needed to read leaf
/// rects.
#[derive(Clone, Copy, Debug)]
pub enum TickOutcome {
    /// Picker still running; keep state, repaint next frame.
    Continue,
    /// User cancelled (Escape, no targets, or invalid source).
    /// Caller should clear its pending-pick state.
    Cancel,
    /// User pressed a target letter. Caller should apply its staged
    /// op against `target` and clear its pending-pick state.
    /// `source` mirrors the value passed in via [`PanePickConfig`].
    Picked {
        /// The source leaf the host originally staged, if any.
        source: Option<NodePath>,
        /// The leaf whose letter was pressed.
        target: NodePath,
    },
}

/// Drive one frame of the picker overlay. Call after the dock has
/// rendered (so leaf viewports are up to date for this frame) and
/// before any other handler that might consume keystrokes.
///
/// `assignments` is a persistent map (`leaf_identity -> letter`)
/// owned by the host. Leaves keep their letter across pick sessions
/// even as the dock around them changes; closing a leaf for good
/// drops its entry so the next new leaf can claim that letter.
///
/// Returns the outcome the caller must act on; this fn never touches
/// the dock itself, it only reads layout and consumes input.
pub fn tick<Tab>(
    ctx: &egui::Context,
    dock: &egui_dock::DockState<Tab>,
    config: PanePickConfig<'_>,
    assignments: &mut BTreeMap<u64, char>,
) -> TickOutcome
where
    Tab: Clone + Hash,
{
    // Cancel immediately if Escape is pressed; even if there are no
    // targets we want this to still close cleanly.
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        return TickOutcome::Cancel;
    }

    // Build the candidate target set, keyed by a stable content hash
    // of each leaf so the assignment table can survive other leaves
    // opening / closing. The visit order matches dock iteration so
    // newly-opened leaves get the lowest free letter available at
    // their position. The optional whitelist restricts the picker to
    // a host-supplied subset.
    let targets: Vec<(NodePath, Rect, u64)> = dock
        .iter_leaves()
        .filter(|(p, _)| config.source.is_none_or(|s| *p != s))
        .filter(|(p, _)| {
            config
                .target_whitelist
                .is_none_or(|allowed| allowed.contains(p))
        })
        .map(|(p, l)| (p, l.rect, leaf_identity(l)))
        .filter(|(_, r, _)| r.is_finite() && r.width() > 1.0 && r.height() > 1.0)
        .collect();
    if targets.is_empty() {
        return TickOutcome::Cancel;
    }

    // Recycle: drop letter assignments whose leaf no longer exists,
    // so a freshly-opened leaf can grab the freed letter.
    let live: HashSet<u64> = targets.iter().map(|(_, _, h)| *h).collect();
    assignments.retain(|k, _| live.contains(k));

    // Two-pass assignment: keep existing letters first (so they stay
    // stable across sessions) then fill in new leaves with the
    // lowest free letter not already claimed.
    let mut taken: HashSet<char> = assignments.values().copied().collect();
    let mut labelled: Vec<(NodePath, Rect, char)> = Vec::with_capacity(targets.len());
    for (path, rect, identity) in &targets {
        if let Some(&letter) = assignments.get(identity) {
            labelled.push((*path, *rect, letter));
        }
    }
    for (path, rect, identity) in &targets {
        if assignments.contains_key(identity) {
            continue;
        }
        let Some(letter) = ('A'..='Z').find(|c| !taken.contains(c)) else {
            // Beyond 26 leaves we'd need two-char labels; silently
            // drop the overflow rather than render unassigned cards.
            break;
        };
        assignments.insert(*identity, letter);
        taken.insert(letter);
        labelled.push((*path, *rect, letter));
    }
    // Render in dock-iteration order so visual pairing is stable
    // across re-runs even if assignments came in a different order.
    labelled.sort_by_key(|(p, _, _)| (p.surface.0, p.node.0));

    // Find the source leaf's rect for the "FROM" badge. iter_leaves
    // is the source of truth for layout this frame. Sourceless picks
    // skip the badge entirely.
    let source_rect = config
        .source
        .and_then(|src| dock.iter_leaves().find(|(p, _)| *p == src).map(|(_, l)| l.rect));

    // Look up which letter (if any) was pressed this frame. Match
    // against the labelled set so we honour the assigned letter
    // rather than positional order.
    let pressed = ctx.input_mut(|i| {
        for (path, _, letter) in labelled.iter() {
            if i.consume_key(Modifiers::NONE, key_for_letter(*letter)) {
                return Some(*path);
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
    egui::Area::new(Id::new("egui-dock-picker-backdrop"))
        .order(Order::Foreground)
        .fixed_pos(ctx.content_rect().min)
        .interactable(true)
        .show(ctx, |ui| {
            let (rect, _resp) =
                ui.allocate_exact_size(ctx.content_rect().size(), egui::Sense::click());
            ui.painter().rect_filled(rect, 0.0, backdrop_fill);
        });

    if let (Some(rect), Some(label)) = (source_rect, config.source_badge_label) {
        paint_source_badge(ctx, rect, label, &visuals);
    }
    for (_, rect, letter) in labelled.iter() {
        paint_target_label(ctx, *rect, *letter, &visuals);
    }

    if let Some(target) = pressed {
        return TickOutcome::Picked {
            source: config.source,
            target,
        };
    }

    // Picker stays active across frames; repaint so a subsequent
    // resize updates the overlay positions immediately.
    ctx.request_repaint();
    TickOutcome::Continue
}

/// Hash of every tab in `leaf`, order-independent (XOR fold) so a
/// user reordering tabs within a leaf doesn't reshuffle its letter.
/// Adding or removing tabs *does* change the identity -- the leaf is
/// conceptually "different" once its tab loadout changes, and that's
/// a cheap price for a single-pass hash. Empty leaves are filtered
/// out before this is called.
fn leaf_identity<Tab: Hash>(leaf: &egui_dock::LeafNode<Tab>) -> u64 {
    let mut combined: u64 = 0;
    for tab in leaf.tabs() {
        let mut h = DefaultHasher::new();
        tab.hash(&mut h);
        combined ^= h.finish();
    }
    combined
}

fn key_for_letter(letter: char) -> Key {
    use Key::*;
    match letter {
        'A' => A,
        'B' => B,
        'C' => C,
        'D' => D,
        'E' => E,
        'F' => F,
        'G' => G,
        'H' => H,
        'I' => I,
        'J' => J,
        'K' => K,
        'L' => L,
        'M' => M,
        'N' => N,
        'O' => O,
        'P' => P,
        'Q' => Q,
        'R' => R,
        'S' => S,
        'T' => T,
        'U' => U,
        'V' => V,
        'W' => W,
        'X' => X,
        'Y' => Y,
        'Z' => Z,
        // Anything outside A-Z is a programming error; map to A as a
        // safe fallback so no input event fires.
        _ => A,
    }
}

fn paint_target_label(ctx: &egui::Context, leaf_rect: Rect, letter: char, visuals: &egui::Visuals) {
    let centre = leaf_rect.center();
    let backdrop = Rect::from_center_size(centre, Vec2::splat(LABEL_BACKDROP_DIAMETER));
    let id = Id::new(("egui-dock-picker-target", letter));
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

fn paint_source_badge(ctx: &egui::Context, leaf_rect: Rect, label: &str, visuals: &egui::Visuals) {
    let centre = leaf_rect.center();
    let id = Id::new("egui-dock-picker-source");
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
