//! Keyboard input flowing through the shared hxy-editor dispatch.
//! Each test mounts a focused [`HexPane`] over a 64-byte memory source
//! and drives it with gpui's `simulate_keystrokes`, asserting on the
//! editor state the dispatch mutated.

use std::sync::Arc;

use gpui::Entity;
use gpui::Focusable;
use gpui::TestAppContext;
use gpui::VisualTestContext;
use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::ColumnCount;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_core::Selection;
use hxy_editor::InputMode;
use hxy_editor::Pane;
use hxy_view_gpui::HexPane;

fn source() -> Arc<dyn HexSource> {
    Arc::new(MemorySource::new(vec![0x00u8; 64]))
}

/// 16 full rows of 16 columns -- enough headroom for the scrolloff
/// test to move the cursor several rows past a synthesized viewport.
fn source_16_rows() -> Arc<dyn HexSource> {
    Arc::new(MemorySource::new(vec![0x00u8; 16 * 16]))
}

fn focus(cx: &mut VisualTestContext, pane: &Entity<HexPane>) {
    cx.update(|window, cx| {
        let handle = pane.read(cx).focus_handle(cx);
        window.focus(&handle);
        window.activate_window();
    });
}

fn seed_caret(cx: &mut VisualTestContext, pane: &Entity<HexPane>, offset: u64) {
    pane.update(cx, |p, _| {
        p.editor_mut().set_selection(Some(Selection::caret(ByteOffset::new(offset))));
    });
}

fn byte_at(cx: &mut VisualTestContext, pane: &Entity<HexPane>, offset: u64) -> u8 {
    pane.read_with(cx, |p, _| {
        let range = ByteRange::new(ByteOffset::new(offset), ByteOffset::new(offset + 1)).unwrap();
        p.editor().source().read(range).unwrap()[0]
    })
}

fn selection(cx: &mut VisualTestContext, pane: &Entity<HexPane>) -> (u64, u64) {
    pane.read_with(cx, |p, _| {
        let sel = p.editor().selection().unwrap();
        (sel.anchor.get(), sel.cursor.get())
    })
}

#[gpui::test]
fn typing_hex_digits_writes_byte_and_advances(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(source(), cx));
    focus(cx, &pane);
    // Hex typing writes at the cursor; without a live selection
    // `type_hex_digit` bails, so place a caret at byte 0 first.
    seed_caret(cx, &pane, 0);

    cx.simulate_keystrokes("a b");

    assert_eq!(byte_at(cx, &pane, 0), 0xAB);
    assert_eq!(selection(cx, &pane).1, 1);
}

#[gpui::test]
fn shift_arrow_extends_selection(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(source(), cx));
    focus(cx, &pane);
    pane.update(cx, |p, _| p.editor_mut().set_active_pane(Pane::Ascii));

    cx.simulate_keystrokes("shift-right shift-right");

    assert_eq!(selection(cx, &pane), (0, 2));
}

#[gpui::test]
fn vim_motion_and_yank_reach_clipboard(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(source(), cx));
    focus(cx, &pane);
    pane.update(cx, |p, _| p.editor_mut().set_input_mode(InputMode::Vim));

    cx.simulate_keystrokes("y y");

    let text = cx.read_from_clipboard().and_then(|item| item.text()).unwrap();
    assert!(text.starts_with("00 00"), "clipboard held {text:?}");
}

#[gpui::test]
fn cmd_modified_keys_pass_through(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(source(), cx));
    focus(cx, &pane);
    // A caret is present, so a stray hex 'a' would land at byte 0 if the
    // command modifier were not honoured.
    seed_caret(cx, &pane, 0);

    cx.simulate_keystrokes("cmd-a");

    assert_eq!(byte_at(cx, &pane, 0), 0x00);
}

#[gpui::test]
fn ascii_pane_typing_inserts_text(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(source(), cx));
    focus(cx, &pane);
    pane.update(cx, |p, _| p.editor_mut().set_active_pane(Pane::Ascii));
    seed_caret(cx, &pane, 0);

    cx.simulate_keystrokes("shift-z");

    assert_eq!(byte_at(cx, &pane, 0), b'Z');
}

/// `HexPane::handle_key_down` must drain `HexEditor`'s scrolloff-driven
/// scroll requests (queued by `ensure_cursor_visible_with_scrolloff`,
/// hxy-editor/src/input.rs:151) and apply them to its own `scroll_rows`,
/// or arrow-key navigation could never scroll the gpui view.
///
/// The bare-root test harness mounts `HexPane` with no ancestor that
/// stretches the canvas to the window (see task-5 report), so a real
/// paint always reports `rows_visible == 0` and `simulate_keystrokes`'s
/// automatic redraw would immediately clobber a one-shot synthesized
/// viewport with that degenerate one. Instead, before each individual
/// keystroke this re-synthesizes a realistic 10-row `on_frame` window
/// anchored at the pane's current `scroll_rows` -- standing in for
/// what a real paint (once its canvas gets a real size) would report.
#[gpui::test]
fn arrow_down_past_scrolloff_scrolls_the_view(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(source_16_rows(), cx));
    focus(cx, &pane);
    seed_caret(cx, &pane, 0);

    let columns = ColumnCount::new(16).unwrap();
    const VISIBLE_ROWS: u64 = 10;

    // Row 0 -> row 8 (8 presses). With a 10-row visible window and the
    // default 3-row scrolloff, the safe zone starts at [3, 7); row 8
    // ends up past the bottom edge, so the dispatcher's
    // `ensure_cursor_visible_with_scrolloff` schedules a scroll-to-byte
    // that `HexPane` must turn into `scroll_rows == 2.0` (hand-traced:
    // presses 1-2 saturate at row 0, 3-6 sit inside the safe zone,
    // press 7 pushes the top to row 1, press 8 -- now against a window
    // re-anchored at row 1 -- pushes it to row 2).
    for _ in 0..8 {
        let top = pane.read_with(cx, |p, _| p.scroll_rows()).floor() as u64;
        let visible = ByteRange::new(ByteOffset::new(top * 16), ByteOffset::new((top + VISIBLE_ROWS) * 16)).unwrap();
        pane.update(cx, |p, _| p.editor_mut().on_frame(0.0, columns, Some(visible), None));
        cx.simulate_keystrokes("down");
    }

    assert_eq!(selection(cx, &pane).1, 8 * 16, "cursor should have moved down 8 rows");
    let scroll_rows = pane.read_with(cx, |p, _| p.scroll_rows());
    assert_eq!(scroll_rows, 2.0);
    // And the cursor row (8) sits inside the resulting scrolloff-safe
    // window [scroll_rows + 3, scroll_rows + 10 - 3) = [5, 9).
    assert!((scroll_rows as u64 + 3..scroll_rows as u64 + 7).contains(&8));
}
