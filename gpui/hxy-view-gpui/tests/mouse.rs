//! Mouse input: caret placement, drag selection, and pane switching.
//! Each test mounts a focused [`HexPane`] over a 64-byte memory source,
//! forces one render so [`hxy_view_gpui::FrameInfo`] is latched, then
//! computes click coordinates from the real geometry before simulating
//! gpui mouse events. Mirrors `tests/keyboard.rs`'s harness pattern.

use std::sync::Arc;

use gpui::Focusable;
use gpui::Modifiers;
use gpui::MouseButton;
use gpui::Pixels;
use gpui::Point;
use gpui::ScrollDelta;
use gpui::ScrollWheelEvent;
use gpui::TestAppContext;
use gpui::TouchPhase;
use gpui::VisualTestContext;
use gpui::point;
use gpui::px;
use hxy_core::ByteOffset;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_core::Selection;
use hxy_editor::NibbleCursor;
use hxy_editor::Pane;
use hxy_view_gpui::FrameInfo;
use hxy_view_gpui::HexPane;

const COLUMNS: u64 = 16;

fn source() -> Arc<dyn HexSource> {
    Arc::new(MemorySource::new(vec![0x00u8; 64]))
}

/// Tall enough (200 rows) that the test window's fallback 768px height
/// cannot show it all, so scroll-dependent geometry is exercised.
fn tall_source() -> Arc<dyn HexSource> {
    Arc::new(MemorySource::new(vec![0x00u8; 200 * 16]))
}

fn focus(cx: &mut VisualTestContext, pane: &gpui::Entity<HexPane>) {
    cx.update(|window, cx| {
        let handle = pane.read(cx).focus_handle(cx);
        window.focus(&handle);
        window.activate_window();
    });
}

/// Forces a render (via a no-op update) and reads back the frame the
/// paint pass latched, so tests can compute click coordinates from the
/// real font metrics instead of guessing pixel values.
fn frame(cx: &mut VisualTestContext, pane: &gpui::Entity<HexPane>) -> FrameInfo {
    pane.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    pane.read_with(cx, |p, _| p.last_frame()).expect("paint should have latched a frame")
}

/// Window position of the hex-pane cell for `offset`, landing inside
/// the glyph pair rather than on its edge.
fn hex_point(frame: &FrameInfo, offset: u64) -> Point<Pixels> {
    let row = offset / COLUMNS;
    let col = (offset % COLUMNS) as u16;
    let x = frame.content_origin.x + frame.geometry.hex_x(col) + px(2.0);
    let y = frame.content_origin.y + frame.geometry.metrics.line_h * (row as f32 + 0.5);
    point(x, y)
}

/// Window position of the ascii-pane cell for `offset`.
fn ascii_point(frame: &FrameInfo, offset: u64) -> Point<Pixels> {
    let row = offset / COLUMNS;
    let col = (offset % COLUMNS) as u16;
    let x = frame.content_origin.x + frame.geometry.ascii_x(col) + px(1.0);
    let y = frame.content_origin.y + frame.geometry.metrics.line_h * (row as f32 + 0.5);
    point(x, y)
}

/// Window position of the hex-pane cell for absolute byte `offset`,
/// accounting for a scrolled `frame` (unlike [`hex_point`], which
/// assumes `first_visible_row == 0`).
fn hex_point_scrolled(frame: &FrameInfo, offset: u64) -> Point<Pixels> {
    let row = offset / COLUMNS;
    let col = (offset % COLUMNS) as u16;
    let visible_row = row.checked_sub(frame.first_visible_row).expect("offset row must be visible");
    let x = frame.content_origin.x + frame.geometry.hex_x(col) + px(2.0);
    let y = frame.content_origin.y + frame.geometry.metrics.line_h * (visible_row as f32 + 0.5);
    point(x, y)
}

/// Scrolls the pane `forward_rows` (fractional) rows toward later
/// content via a real scroll-wheel event, then forces a render and
/// returns the new frame. gpui's wheel delta is negative in that
/// direction (see `on_scroll_wheel`'s comment in pane.rs), so this
/// negates `forward_rows` before building the event.
fn scroll_by_rows(cx: &mut VisualTestContext, pane: &gpui::Entity<HexPane>, at: Point<Pixels>, forward_rows: f32) -> FrameInfo {
    cx.simulate_event(ScrollWheelEvent {
        position: at,
        delta: ScrollDelta::Lines(point(0.0, -forward_rows)),
        modifiers: Modifiers::none(),
        touch_phase: TouchPhase::Moved,
    });
    frame(cx, pane)
}

fn selection(cx: &mut VisualTestContext, pane: &gpui::Entity<HexPane>) -> (u64, u64) {
    pane.read_with(cx, |p, _| {
        let sel = p.editor().selection().unwrap();
        (sel.anchor.get(), sel.cursor.get())
    })
}

#[gpui::test]
fn click_sets_caret_and_pane(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(source(), cx));
    focus(cx, &pane);
    let frame = frame(cx, &pane);

    let pos = hex_point(&frame, 5);
    cx.simulate_click(pos, Modifiers::none());

    assert_eq!(selection(cx, &pane), (5, 5));
    assert_eq!(pane.read_with(cx, |p, _| p.editor().active_pane()), Pane::Hex);
}

#[gpui::test]
fn click_ascii_cell_switches_pane(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(source(), cx));
    focus(cx, &pane);
    let frame = frame(cx, &pane);

    let pos = ascii_point(&frame, 9);
    cx.simulate_click(pos, Modifiers::none());

    assert_eq!(selection(cx, &pane), (9, 9));
    assert_eq!(pane.read_with(cx, |p, _| p.editor().active_pane()), Pane::Ascii);
}

#[gpui::test]
fn drag_extends_selection(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(source(), cx));
    focus(cx, &pane);
    let frame = frame(cx, &pane);

    let down = hex_point(&frame, 3);
    let moved = hex_point(&frame, 12);

    cx.simulate_mouse_down(down, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(moved, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(moved, MouseButton::Left, Modifiers::none());

    assert_eq!(selection(cx, &pane), (3, 12));
}

/// Regression: `hit_at` must land on the right byte after a scroll
/// with a fractional row offset, not just from the top of the file.
#[gpui::test]
fn click_after_fractional_scroll_hits_correct_byte(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(tall_source(), cx));
    focus(cx, &pane);
    let frame1 = frame(cx, &pane);

    let frame2 = scroll_by_rows(cx, &pane, point(frame1.content_origin.x + px(5.0), frame1.content_origin.y + px(5.0)), 2.5);
    assert_eq!(frame2.first_visible_row, 2);

    let offset = 5 * COLUMNS + 4;
    cx.simulate_click(hex_point_scrolled(&frame2, offset), Modifiers::none());

    assert_eq!(selection(cx, &pane).0, offset);
}

/// Regression: the auto-scroll trigger edge must track the widget's
/// true screen-space top, not `FrameInfo::content_origin` directly
/// (which paint.rs shifts up by the scroll position's fractional
/// part). A point 1px below `content_origin.y` sits inside the old,
/// uncorrected boundary but is still above the true, frac-corrected
/// top edge, so it must still trigger an upward auto-scroll.
#[gpui::test]
fn drag_above_true_top_after_fractional_scroll_scrolls_up(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(tall_source(), cx));
    focus(cx, &pane);
    let frame1 = frame(cx, &pane);

    let frame2 = scroll_by_rows(cx, &pane, point(frame1.content_origin.x + px(5.0), frame1.content_origin.y + px(5.0)), 2.5);
    assert_eq!(frame2.first_visible_row, 2);

    let down = hex_point_scrolled(&frame2, 3 * COLUMNS);
    let above_buggy_top = point(down.x, frame2.content_origin.y + px(1.0));

    cx.simulate_mouse_down(down, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(above_buggy_top, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(above_buggy_top, MouseButton::Left, Modifiers::none());

    let frame3 = frame(cx, &pane);
    assert_eq!(frame3.first_visible_row, 1, "point above the true (frac-corrected) top edge must auto-scroll up");
}

/// egui parity: a drag that starts and ends on the SAME byte must not
/// disturb a pending half-typed nibble. Reference: `hxy-view/src/lib.rs`'s
/// `apply_interaction` `held` branch mutates `Selection::cursor` directly
/// through `view_parts`, and `hxy-editor`'s external-cursor-move
/// reconciliation (input.rs:87-94) only resets the nibble when the
/// cursor's byte actually changes since the last dispatch -- never
/// touched by a same-byte drag. This is the case `set_selection` (which
/// unconditionally resets the nibble) used to fail.
#[gpui::test]
fn drag_on_same_byte_preserves_pending_nibble(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(source(), cx));
    focus(cx, &pane);
    let frame = frame(cx, &pane);

    pane.update(cx, |p, _| p.editor_mut().set_selection(Some(Selection::caret(ByteOffset::new(0)))));

    // One hex digit writes the high nibble and leaves the cursor on
    // byte 0 with the low nibble pending.
    cx.simulate_keystrokes("a");
    assert_eq!(pane.read_with(cx, |p, _| p.editor().nibble()), Some(NibbleCursor::Low));

    let pos = hex_point(&frame, 0);
    cx.simulate_mouse_down(pos, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(pos, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(pos, MouseButton::Left, Modifiers::none());

    assert_eq!(selection(cx, &pane), (0, 0));
    assert_eq!(pane.read_with(cx, |p, _| p.editor().nibble()), Some(NibbleCursor::Low));
}

/// 1000 full rows of 16 columns, tall enough that clicking the middle
/// of the minimap strip should center the viewport around row 500.
fn thousand_rows_source() -> Arc<dyn HexSource> {
    Arc::new(MemorySource::new(vec![0x00u8; 1000 * 16]))
}

/// Clicking the minimap strip at 50% height should scroll the grid so
/// the byte-row roughly halfway through the file (row 500 of 1000)
/// ends up centered in the viewport, within one row. The strip's
/// midpoint centering leaves room below the max-scroll clamp for this
/// file, so no clamp interferes.
#[gpui::test]
fn minimap_click_scrolls_viewport(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(thousand_rows_source(), cx));
    focus(cx, &pane);
    let frame = frame(cx, &pane);
    let strip = frame.minimap_bounds;

    let click = point(strip.origin.x + strip.size.width * 0.5, strip.origin.y + strip.size.height * 0.5);
    cx.simulate_click(click, Modifiers::none());

    let scroll_rows = pane.read_with(cx, |p, _| p.scroll_rows());
    let centered_row = scroll_rows + frame.rows_visible / 2.0;
    assert!((centered_row - 500.0).abs() <= 1.0, "expected centered row near 500, got {centered_row}");
}


/// Fix 1 proof: with the canvas sized to fill the pane, a real paint
/// in the test harness measures a nonzero viewport instead of the
/// zero-height content mask a childless canvas would produce.
#[gpui::test]
fn paint_sizes_the_canvas(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(tall_source(), cx));
    focus(cx, &pane);
    let frame = frame(cx, &pane);

    assert!(frame.rows_visible > 0.0, "sized canvas should paint a nonzero viewport, got {}", frame.rows_visible);
    assert!(frame.minimap_bounds.size.height > px(0.0), "minimap strip should have a real height");
}

/// egui parity: a plain click sets a caret, then a shift-click keeps
/// the original anchor and moves the cursor to the new hit, extending
/// the selection. Reference: `apply_interaction`'s press branch
/// (hxy-view/src/lib.rs:2349-2358).
#[gpui::test]
fn shift_click_extends_selection(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(source(), cx));
    focus(cx, &pane);
    let frame = frame(cx, &pane);

    cx.simulate_click(hex_point(&frame, 2), Modifiers::none());
    assert_eq!(selection(cx, &pane), (2, 2));

    cx.simulate_click(hex_point(&frame, 10), Modifiers { shift: true, ..Modifiers::none() });
    assert_eq!(selection(cx, &pane), (2, 10));
}
