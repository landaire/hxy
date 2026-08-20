//! Row-map, hover-span, and byte-styler layers. Each test mounts a
//! focused [`HexPane`], installs one of the layers, forces a render so
//! the paint pass exercises it, and asserts on the pane's observable
//! state (pixels are unassertable, so the smoke is "paint ran without
//! panic" plus a state check where one exists). Mirrors the harness in
//! `tests/mouse.rs`.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use gpui::Focusable;
use gpui::Modifiers;
use gpui::Pixels;
use gpui::Point;
use gpui::TestAppContext;
use gpui::VisualTestContext;
use gpui::hsla;
use gpui::point;
use gpui::px;
use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_core::RowSlot;
use hxy_view_gpui::ByteStyleOverride;
use hxy_view_gpui::FrameInfo;
use hxy_view_gpui::HexPane;

fn source() -> Arc<dyn HexSource> {
    // Sequential bytes so styler-offset assertions can distinguish cells.
    Arc::new(MemorySource::new((0..=255u8).collect::<Vec<_>>()))
}

fn focus(cx: &mut VisualTestContext, pane: &gpui::Entity<HexPane>) {
    cx.update(|window, cx| {
        let handle = pane.read(cx).focus_handle(cx);
        window.focus(&handle);
        window.activate_window();
    });
}

fn frame(cx: &mut VisualTestContext, pane: &gpui::Entity<HexPane>) -> FrameInfo {
    pane.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    pane.read_with(cx, |p, _| p.last_frame()).expect("paint should have latched a frame")
}

/// Window position inside the hex cell at `visual_row` / `col`, using
/// the visual row directly (the linear `offset / cols` mapping does not
/// hold under a row map).
fn hex_point_at(frame: &FrameInfo, visual_row: u64, col: u16) -> Point<Pixels> {
    let x = frame.content_origin.x + frame.geometry.hex_x(col) + px(2.0);
    let y = frame.content_origin.y + frame.geometry.metrics.line_h * (visual_row as f32 + 0.5);
    point(x, y)
}

fn selection(cx: &mut VisualTestContext, pane: &gpui::Entity<HexPane>) -> Option<(u64, u64)> {
    pane.read_with(cx, |p, _| p.editor().selection().map(|s| (s.anchor.get(), s.cursor.get())))
}

/// A row map whose second visual row is a gap paints (no panic) and a
/// [`RowSlot::Real`] row's click lands on the slot's offset, not the
/// linear `row * cols`.
#[gpui::test]
fn row_map_real_row_click_uses_slot_offset(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(source(), cx));
    focus(cx, &pane);
    // Row 0: bytes 0..16. Row 1: gap. Row 2: bytes 128..144.
    pane.update(cx, |p, cx| {
        p.set_row_map(Some(vec![RowSlot::real(0, 16), RowSlot::Gap, RowSlot::real(128, 16)]), cx);
    });
    let frame = frame(cx, &pane);

    // Column 3 of visual row 2 is byte 131, not 2*16+3 = 35.
    cx.simulate_click(hex_point_at(&frame, 2, 3), Modifiers::none());
    assert_eq!(selection(cx, &pane), Some((131, 131)));
}

/// Clicking a [`RowSlot::Gap`] row is a no-hit: the selection set on a
/// prior real-row click stays put.
#[gpui::test]
fn row_map_gap_row_click_is_noop(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(source(), cx));
    focus(cx, &pane);
    pane.update(cx, |p, cx| {
        p.set_row_map(Some(vec![RowSlot::real(0, 16), RowSlot::Gap, RowSlot::real(128, 16)]), cx);
    });
    let frame = frame(cx, &pane);

    cx.simulate_click(hex_point_at(&frame, 0, 5), Modifiers::none());
    assert_eq!(selection(cx, &pane), Some((5, 5)));

    // Row 1 is the gap: the click resolves to no byte and leaves the
    // selection unchanged.
    cx.simulate_click(hex_point_at(&frame, 1, 5), Modifiers::none());
    assert_eq!(selection(cx, &pane), Some((5, 5)));
}

/// A partial [`RowSlot::Real`] row (fewer than `cols` bytes) paints and
/// a click past its real bytes is a no-hit.
#[gpui::test]
fn row_map_partial_row_tail_click_is_noop(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(source(), cx));
    focus(cx, &pane);
    // Single 4-byte row.
    pane.update(cx, |p, cx| p.set_row_map(Some(vec![RowSlot::real(10, 4)]), cx));
    let frame = frame(cx, &pane);

    cx.simulate_click(hex_point_at(&frame, 0, 2), Modifiers::none());
    assert_eq!(selection(cx, &pane), Some((12, 12)));

    // Column 9 is past the 4-byte slot: no hit, selection unchanged.
    cx.simulate_click(hex_point_at(&frame, 0, 9), Modifiers::none());
    assert_eq!(selection(cx, &pane), Some((12, 12)));
}

/// An empty row map paints without panicking (row count 0, no slots to
/// read): the mapped path must tolerate the degenerate config.
#[gpui::test]
fn empty_row_map_paints(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(source(), cx));
    focus(cx, &pane);
    pane.update(cx, |p, cx| p.set_row_map(Some(Vec::new()), cx));
    let _ = frame(cx, &pane);
}

/// Installing a hover span paints the secondary band without panic.
#[gpui::test]
fn hover_span_paints(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(source(), cx));
    focus(cx, &pane);
    let span = ByteRange::new(ByteOffset::new(4), ByteOffset::new(12)).unwrap();
    pane.update(cx, |p, cx| p.set_hover_span(Some(span), cx));
    // Latches a frame => the hover-band paint path ran over the span.
    let _ = frame(cx, &pane);
}

/// Installing a custom byte-value palette paints without panic (the
/// per-glyph color resolution is unit-tested in paint.rs; this smokes
/// the full pass with a palette in the snapshot).
#[gpui::test]
fn value_palette_paints(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(source(), cx));
    focus(cx, &pane);
    pane.update(cx, |p, cx| {
        p.set_value_palette(Some(Arc::new([hsla(0.1, 0.5, 0.5, 1.0); 256])), cx);
    });
    let _ = frame(cx, &pane);
}

/// The byte styler is consulted for on-screen cells during paint.
#[gpui::test]
fn byte_styler_consulted_during_paint(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(source(), cx));
    focus(cx, &pane);

    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(AtomicUsize::new(0));
    let calls_c = calls.clone();
    let seen_c = seen.clone();
    pane.update(cx, |p, cx| {
        p.set_byte_styler(
            Some(Box::new(move |byte, offset| {
                calls_c.fetch_add(1, Ordering::Relaxed);
                // Record having seen the byte at offset 3 with its value.
                if offset.get() == 3 && byte == 3 {
                    seen_c.fetch_add(1, Ordering::Relaxed);
                }
                ByteStyleOverride { bg: Some(hsla(0.5, 0.5, 0.5, 1.0)), fg: Some(hsla(0.0, 0.0, 1.0, 1.0)) }
            })),
            cx,
        );
    });
    let _ = frame(cx, &pane);

    assert!(calls.load(Ordering::Relaxed) > 0, "styler must be consulted during paint");
    assert!(seen.load(Ordering::Relaxed) > 0, "styler must receive each byte's value and absolute offset");
}
