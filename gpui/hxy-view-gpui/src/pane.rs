//! [`HexPane`]: a focusable gpui entity that owns a [`HexEditor`] and
//! renders it through the canvas paint pass in [`crate::paint`].

use std::sync::Arc;

use gpui::App;
use gpui::ClipboardItem;
use gpui::Context;
use gpui::FocusHandle;
use gpui::Focusable;
use gpui::InteractiveElement;
use gpui::IntoElement;
use gpui::KeyDownEvent;
use gpui::MouseButton;
use gpui::MouseDownEvent;
use gpui::MouseMoveEvent;
use gpui::MouseUpEvent;
use gpui::ParentElement;
use gpui::Pixels;
use gpui::Point;
use gpui::Render;
use gpui::ScrollDelta;
use gpui::ScrollWheelEvent;
use gpui::Styled;
use gpui::Window;
use gpui::div;
use gpui::point;
use gpui::px;
use hxy_editor::Disposition;
use hxy_editor::Effect;
use gpui_component::ActiveTheme;
use hxy_core::ByteOffset;
use hxy_core::ColumnCount;
use hxy_core::HexSource;
use hxy_core::Selection;

use crate::GridGeometry;
use crate::GridHit;
use crate::MinimapBounds;
use crate::paint::GridSnapshot;
use crate::paint::PaintColors;
use crate::paint::hex_canvas;

/// Row height fallback used to convert pixel scroll deltas before the
/// first frame measures the real line height.
const FALLBACK_LINE_H: f32 = 16.0;

/// Geometry and viewport facts latched during paint, consumed by input
/// handlers (hit testing, page-size scrolling).
#[derive(Clone, Copy, Debug)]
pub struct FrameInfo {
    pub geometry: GridGeometry,
    pub content_origin: gpui::Point<Pixels>,
    pub rows_visible: f32,
    pub first_visible_row: u64,
    pub minimap_bounds: MinimapBounds,
}

/// A hex-editor viewport entity. Holds the editor model, keyboard focus,
/// the column count, and vertical scroll in fractional rows.
pub struct HexPane {
    editor: hxy_editor::HexEditor,
    focus_handle: FocusHandle,
    columns: ColumnCount,
    /// Vertical scroll in fractional rows (row 0 at top when 0.0).
    scroll_rows: f32,
    /// Set during paint, consumed by input handlers.
    pub(crate) last_frame: Option<FrameInfo>,
    /// Byte the left button went down on; `Some` for the duration of a
    /// drag, pinning the selection anchor while the cursor follows the
    /// pointer. `None` when no drag is in progress.
    drag_anchor: Option<ByteOffset>,
    /// `true` for the duration of a press-and-drag that started on the
    /// minimap strip. Scrubs `scroll_rows` continuously while held and
    /// never touches `drag_anchor` or the selection.
    minimap_scrubbing: bool,
}

impl HexPane {
    pub fn new(source: Arc<dyn HexSource>, cx: &mut Context<Self>) -> Self {
        Self {
            editor: hxy_editor::HexEditor::new(source),
            focus_handle: cx.focus_handle(),
            columns: ColumnCount::DEFAULT,
            scroll_rows: 0.0,
            last_frame: None,
            drag_anchor: None,
            minimap_scrubbing: false,
        }
    }

    pub fn editor(&self) -> &hxy_editor::HexEditor {
        &self.editor
    }

    /// Mutable editor access. Callers that mutate must `cx.notify()` to
    /// schedule a repaint.
    pub fn editor_mut(&mut self) -> &mut hxy_editor::HexEditor {
        &mut self.editor
    }

    /// The hex view's current column count for this pane.
    pub fn columns(&self) -> ColumnCount {
        self.columns
    }

    /// Set the hex view's column count for this pane and repaint. Used
    /// by the command palette's `Set columns...` mode.
    pub fn set_columns(&mut self, columns: ColumnCount, cx: &mut Context<Self>) {
        self.columns = columns;
        cx.notify();
    }

    pub fn set_source(&mut self, source: Arc<dyn HexSource>, cx: &mut Context<Self>) {
        self.editor = hxy_editor::HexEditor::new(source);
        self.scroll_rows = 0.0;
        self.last_frame = None;
        cx.notify();
    }

    /// Total rows including the trailing EOF-cursor row.
    fn row_count(&self) -> u64 {
        let cols = self.columns.as_u64();
        self.editor.source().len().get().saturating_add(1).div_ceil(cols).max(1)
    }

    fn line_height(&self) -> Pixels {
        self.last_frame.map(|f| f.geometry.metrics.line_h).unwrap_or(px(FALLBACK_LINE_H))
    }

    /// Largest scroll offset (in rows) that still fills the viewport:
    /// the last row stops at the viewport bottom instead of parking at
    /// the top with blank space below (egui's overscroll clamp). Once a
    /// frame is latched, subtract the visible-row count; pre-first-paint
    /// there is no viewport height, so fall back to `row_count - 1`.
    fn max_scroll_rows(&self) -> f32 {
        match self.last_frame {
            Some(frame) => (self.row_count() as f32 - frame.rows_visible).max(0.0),
            None => self.row_count().saturating_sub(1) as f32,
        }
    }

    fn on_scroll_wheel(&mut self, ev: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let delta_rows = scroll_delta_to_rows(ev.delta, self.line_height());
        let max = self.max_scroll_rows();
        // gpui scroll deltas are negative when the wheel moves toward
        // later content, which should advance the top row.
        self.scroll_rows = (self.scroll_rows - delta_rows).clamp(0.0, max);
        cx.notify();
    }

    /// Feed one key-down through the editor. Returns true when the
    /// editor consumed it (callers stop propagation).
    ///
    /// Mirrors the egui adapter's dispatch contract: translate the
    /// keystroke into a key event plus the synthesized text event, feed
    /// both through one input filter, then apply the batch. `apply_input`
    /// runs UNCONDITIONALLY -- even an empty batch advances the editor's
    /// once-per-frame bookkeeping (external-cursor-move detection,
    /// history boundaries).
    ///
    /// One intentional divergence from egui: the egui adapter drains a
    /// whole frame of queued events into a single filter/apply batch,
    /// whereas each gpui key-down is its own single-event batch. The
    /// editor's per-event dispatch and the Insert/Replace Escape latch
    /// only matter within one batch, so single-event batches stay
    /// semantically safe -- Escape still pops the mode; there are simply
    /// no same-batch followers for the latch to drop.
    pub(crate) fn handle_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) -> bool {
        let (key_event, text_event) = crate::input::translate(&event.keystroke);
        let mut filter = self.editor.input_filter();
        let mut consumed = false;
        for event in [key_event, text_event].into_iter().flatten() {
            consumed |= filter.feed(&event) == Disposition::Consumed;
        }
        for effect in self.editor.apply_input(filter.finish()) {
            match effect {
                Effect::CopyText(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            }
        }
        // `apply_input` can queue a scroll request (e.g. arrow-key
        // navigation tripping `ensure_cursor_visible_with_scrolloff`,
        // or an edit re-pinning the current position). Drain and apply
        // it the same way the mouse handlers do.
        let (pending_scroll, pending_scroll_to_byte) = {
            let parts = self.editor.view_parts();
            (parts.pending_scroll, parts.pending_scroll_to_byte)
        };
        self.apply_pending_scroll(pending_scroll, pending_scroll_to_byte);
        cx.notify();
        consumed
    }

    /// Map a window position through the latched [`FrameInfo`] into a
    /// grid hit. `None` before the first paint (no frame latched yet)
    /// or when the position falls outside both panes.
    fn hit_at(&self, position: Point<Pixels>) -> Option<GridHit> {
        let frame = self.last_frame?;
        let line_h = frame.geometry.metrics.line_h;
        let x = position.x - frame.content_origin.x;
        let y = position.y - frame.content_origin.y + line_h * frame.first_visible_row as f32;
        frame.geometry.hit_test(point(x, y), self.editor.source().len())
    }

    /// `true` when `x` falls inside this frame's minimap strip.
    /// Checked before grid hit-testing so a click on the strip never
    /// reaches [`Self::hit_at`].
    fn in_minimap_strip(&self, x: Pixels) -> bool {
        let Some(frame) = self.last_frame else { return false };
        let b = frame.minimap_bounds;
        x >= b.origin.x && x < b.origin.x + b.size.width
    }

    /// Maps a y position inside the minimap strip to a scroll target
    /// that centers the corresponding file location in the viewport.
    /// No-op before the first paint or if the strip has no height.
    fn scrub_minimap(&mut self, y: Pixels) {
        let Some(frame) = self.last_frame else { return };
        let b = frame.minimap_bounds;
        if b.size.height <= px(0.0) {
            return;
        }
        let frac = ((y - b.origin.y) / b.size.height).clamp(0.0, 1.0);
        let target_row = frac * self.row_count() as f32;
        let max = self.max_scroll_rows();
        self.scroll_rows = (target_row - frame.rows_visible / 2.0).clamp(0.0, max);
    }

    /// Left-down: place the caret at the hit, switch the active pane,
    /// start a drag, and take keyboard focus. Mirrors egui's
    /// `apply_interaction` press branch (hxy-view/src/lib.rs:2349-2358):
    /// a plain press writes `Selection::caret(hit_offset)`; a
    /// shift-press over an existing selection keeps its anchor and
    /// moves the cursor to the hit. Written through the same
    /// mutable-borrow mechanism egui uses
    /// (`HexEditor::view_parts().selection`), not `set_selection`. The
    /// nibble/history-break reset egui gets "for free" isn't a property
    /// of `set_selection` -- it comes from the input dispatcher's
    /// edge-triggered external-cursor-move check at the top of `apply`
    /// (hxy-editor/src/input.rs:87-94), which compares the cursor byte
    /// against `last_cursor_offset` on the *next* keystroke. Going
    /// through `view_parts` here reproduces that: the reset (if any)
    /// happens on the next keystroke, not synchronously on click.
    fn handle_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.in_minimap_strip(event.position.x) {
            self.minimap_scrubbing = true;
            self.scrub_minimap(event.position.y);
            window.focus(&self.focus_handle);
            cx.notify();
            return;
        }
        let Some(hit) = self.hit_at(event.position) else { return };
        self.editor.set_active_pane(hit.pane);
        let anchor;
        let (pending_scroll, pending_scroll_to_byte) = {
            let parts = self.editor.view_parts();
            let selection = match (event.modifiers.shift, *parts.selection) {
                (true, Some(existing)) => Selection { anchor: existing.anchor, cursor: hit.offset },
                _ => Selection::caret(hit.offset),
            };
            anchor = selection.anchor;
            *parts.selection = Some(selection);
            (parts.pending_scroll, parts.pending_scroll_to_byte)
        };
        // A shift-extended selection keeps its original anchor, so a
        // drag that follows continues from there rather than the click.
        self.drag_anchor = Some(anchor);
        self.apply_pending_scroll(pending_scroll, pending_scroll_to_byte);
        window.focus(&self.focus_handle);
        cx.notify();
    }

    /// Move-with-left-held: extend the cursor to the hit under the
    /// pointer while the anchor stays pinned at the press byte, and
    /// auto-scroll one row when the pointer strays above or below the
    /// grid. Mirrors egui's `apply_interaction` held branch
    /// (hxy-view/src/lib.rs:1823-1826): `s.cursor = hit_offset` through
    /// `view_parts().selection`, leaving the nibble/history-break state
    /// untouched (see [`Self::handle_mouse_down`]'s doc for why that's
    /// still parity-correct rather than a bypass).
    fn handle_mouse_move(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.minimap_scrubbing {
            if event.pressed_button == Some(MouseButton::Left) {
                self.scrub_minimap(event.position.y);
                cx.notify();
            }
            return;
        }
        let Some(anchor) = self.drag_anchor else { return };
        if event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let scrolled = self.auto_scroll_for_drag(event.position);
        let Some(hit) = self.hit_at(event.position) else {
            if scrolled {
                cx.notify();
            }
            return;
        };
        let (pending_scroll, pending_scroll_to_byte) = {
            let parts = self.editor.view_parts();
            *parts.selection = Some(Selection { anchor, cursor: hit.offset });
            (parts.pending_scroll, parts.pending_scroll_to_byte)
        };
        self.apply_pending_scroll(pending_scroll, pending_scroll_to_byte);
        // A hit always extends the selection (above), so this branch
        // always repaints regardless of whether the scroll also moved.
        cx.notify();
    }

    /// Left-up: end the drag or the minimap scrub.
    fn handle_mouse_up(&mut self, _event: &MouseUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        self.drag_anchor = None;
        self.minimap_scrubbing = false;
    }

    /// Scrolls one row toward the pointer when it is above or below the
    /// grid's content area, clamped to the row range. Returns whether
    /// the scroll position changed.
    ///
    /// `FrameInfo::content_origin` is shifted up by the fractional part
    /// of the scroll position (paint.rs bakes that in so `hit_at`'s row
    /// math lines up); undo that shift here to get the widget's actual
    /// fixed screen-space top edge, or a mid-scroll drag would trigger
    /// auto-scroll while the pointer is still visually inside the grid.
    fn auto_scroll_for_drag(&mut self, position: Point<Pixels>) -> bool {
        let Some(frame) = self.last_frame else { return false };
        let line_h = frame.geometry.metrics.line_h;
        let frac = self.scroll_rows - frame.first_visible_row as f32;
        let grid_top = frame.content_origin.y + line_h * frac;
        let grid_bottom = grid_top + line_h * frame.rows_visible;
        let delta = if position.y < grid_top {
            -1.0
        } else if position.y > grid_bottom {
            1.0
        } else {
            return false;
        };
        let max = self.max_scroll_rows();
        let scroll_rows = (self.scroll_rows + delta).clamp(0.0, max);
        if scroll_rows == self.scroll_rows {
            return false;
        }
        self.scroll_rows = scroll_rows;
        true
    }

    /// Applies a scroll request drained from `HexEditor::view_parts()`
    /// (`set_scroll_to` / `set_scroll_to_byte`, e.g. from
    /// `ensure_cursor_visible_with_scrolloff`) onto the pane's
    /// row-based scroll. A byte target wins over a pixel target when
    /// both are queued in the same batch, matching how
    /// `ensure_cursor_visible_with_scrolloff`'s byte-target request can
    /// coexist with an edit's re-pinned pixel offset in one dispatch.
    /// A pixel target is dropped if no frame has painted yet (no known
    /// line height to divide by); a byte target needs no line height,
    /// so it still works pre-paint. Returns whether the scroll changed.
    fn apply_pending_scroll(&mut self, pending_scroll: Option<f32>, pending_scroll_to_byte: Option<ByteOffset>) -> bool {
        let target_rows = if let Some(byte) = pending_scroll_to_byte {
            Some((byte.get() / self.columns.as_u64()) as f32)
        } else if let Some(offset_px) = pending_scroll {
            self.last_frame.map(|frame| offset_px / f32::from(frame.geometry.metrics.line_h))
        } else {
            None
        };
        let Some(target_rows) = target_rows else { return false };
        let max = self.max_scroll_rows();
        let clamped = target_rows.clamp(0.0, max);
        if clamped == self.scroll_rows {
            return false;
        }
        self.scroll_rows = clamped;
        true
    }

    /// Current vertical scroll, in fractional rows. Same visibility as
    /// [`Self::last_frame`]: integration tests need it to assert on
    /// scrolloff/auto-scroll behavior.
    pub fn scroll_rows(&self) -> f32 {
        self.scroll_rows
    }

    /// The last frame's latched geometry. Lets integration tests (and
    /// any future consumer) compute click coordinates from real font
    /// metrics; same visibility as [`Self::editor`].
    pub fn last_frame(&self) -> Option<FrameInfo> {
        self.last_frame
    }

    /// Applies any scroll request the editor queued (`set_scroll_to` /
    /// `set_scroll_to_byte`) and repaints. Key-driven navigation gets
    /// this for free via [`Self::handle_key_down`]; callers that move
    /// the selection programmatically -- search match jumps, goto --
    /// must call this afterward or the pane's scroll position won't
    /// follow the new selection.
    pub fn sync_pending_scroll(&mut self, cx: &mut Context<Self>) {
        let (pending_scroll, pending_scroll_to_byte) = {
            let parts = self.editor.view_parts();
            (parts.pending_scroll, parts.pending_scroll_to_byte)
        };
        self.apply_pending_scroll(pending_scroll, pending_scroll_to_byte);
        cx.notify();
    }
}

/// Pure delta-to-rows conversion. `Lines` deltas are rows already;
/// `Pixels` deltas divide by the row height.
fn scroll_delta_to_rows(delta: ScrollDelta, line_h: Pixels) -> f32 {
    match delta {
        ScrollDelta::Lines(p) => p.y,
        ScrollDelta::Pixels(p) => f32::from(p.y) / f32::from(line_h),
    }
}

impl Focusable for HexPane {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for HexPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snap = GridSnapshot {
            source: self.editor.source().clone(),
            selection: self.editor.selection(),
            active_pane: self.editor.active_pane(),
            nibble: self.editor.nibble(),
            columns: self.columns,
            scroll_rows: self.scroll_rows,
            colors: PaintColors::from_theme(cx.theme()),
            mono_family: cx.theme().mono_font_family.clone(),
            mono_size: cx.theme().mono_font_size,
        };
        let canvas = hex_canvas(snap, cx.entity());

        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().background)
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .on_key_down(cx.listener(|this, event, window, cx| {
                if this.handle_key_down(event, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::handle_mouse_down))
            .on_mouse_move(cx.listener(Self::handle_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::handle_mouse_up))
            .child(div().size_full().child(canvas))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use gpui::TouchPhase;
    use gpui::point;
    use hxy_core::MemorySource;

    /// 64 full rows of 16 columns.
    fn source_64_rows() -> Arc<dyn HexSource> {
        Arc::new(MemorySource::new(vec![0u8; 64 * 16]))
    }

    fn lines_event(dy: f32) -> ScrollWheelEvent {
        ScrollWheelEvent {
            position: point(px(20.0), px(20.0)),
            delta: ScrollDelta::Lines(point(0.0, dy)),
            modifiers: Default::default(),
            touch_phase: TouchPhase::Moved,
        }
    }

    #[test]
    fn lines_delta_is_rows_pixels_delta_divides_by_line_height() {
        assert_eq!(scroll_delta_to_rows(ScrollDelta::Lines(point(0.0, -3.0)), px(16.0)), -3.0);
        assert_eq!(scroll_delta_to_rows(ScrollDelta::Pixels(point(px(0.0), px(-32.0))), px(16.0)), -2.0);
    }

    #[gpui::test]
    fn scroll_wheel_moves_and_clamps(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|_window, cx| HexPane::new(source_64_rows(), cx));
        let pane = window.root(cx).unwrap();
        // Force a real paint so a frame is latched; the overscroll clamp
        // then keeps the last row at the viewport bottom.
        window.update(cx, |_, _, cx| cx.notify()).unwrap();
        cx.run_until_parked();

        // Wheel toward later content advances the top row.
        window.update(cx, |p, window, cx| p.on_scroll_wheel(&lines_event(-3.0), window, cx)).unwrap();
        assert_eq!(pane.read_with(cx, |p, _| p.scroll_rows()), 3.0);

        // A large forward scroll clamps back to the top.
        window.update(cx, |p, window, cx| p.on_scroll_wheel(&lines_event(1_000.0), window, cx)).unwrap();
        assert_eq!(pane.read_with(cx, |p, _| p.scroll_rows()), 0.0);

        // A large backward scroll clamps so the last row stops at the
        // viewport bottom (overscroll parity), short of row_count - 1.
        window.update(cx, |p, window, cx| p.on_scroll_wheel(&lines_event(-1_000.0), window, cx)).unwrap();
        let (scroll, max, floor) =
            window.update(cx, |p, _, _| (p.scroll_rows(), p.max_scroll_rows(), p.row_count().saturating_sub(1) as f32)).unwrap();
        assert_eq!(scroll, max);
        assert!(max < floor, "overscroll clamp must stop short of row_count - 1 once a frame is latched");
    }
}
