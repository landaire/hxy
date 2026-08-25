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
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_core::Selection;
use hxy_editor::InputMode;
use hxy_editor::Pane;
use hxy_view_gpui::HexPane;

fn source() -> Arc<dyn HexSource> {
    Arc::new(MemorySource::new(vec![0x00u8; 64]))
}

/// 200 full rows of 16 columns -- taller than the test window's real
/// viewport, so the scrolloff test can drive the cursor past the bottom
/// margin and force a real scroll.
fn source_200_rows() -> Arc<dyn HexSource> {
    Arc::new(MemorySource::new(vec![0x00u8; 200 * 16]))
}

fn focus(cx: &mut VisualTestContext, pane: &Entity<HexPane>) {
    cx.update(|window, cx| {
        let handle = pane.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
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
/// The source is taller than the test window's real viewport, so a real
/// paint feeds the editor a viewport shorter than the content. Driving
/// the cursor down past the bottom scrolloff margin forces the shared
/// dispatch to schedule a scroll that `HexPane` applies each keystroke;
/// this exercises the real integrated path, no synthesized frame.
#[gpui::test]
fn arrow_down_past_scrolloff_scrolls_the_view(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(source_200_rows(), cx));
    focus(cx, &pane);
    seed_caret(cx, &pane, 0);

    // The default scrolloff margin the editor keeps between the cursor
    // and each viewport edge.
    const SCROLLOFF: u64 = 3;

    let rows_visible = pane.read_with(cx, |p, _| p.last_frame().unwrap().rows_visible);
    let visible_rows = rows_visible.ceil() as u64;
    // Move well past the bottom margin so a scroll is unavoidable.
    let downs = visible_rows + 20;
    for _ in 0..downs {
        cx.simulate_keystrokes("down");
    }

    let cursor_row = selection(cx, &pane).1 / 16;
    assert_eq!(cursor_row, downs, "cursor should move down one row per press");

    let scroll_rows = pane.read_with(cx, |p, _| p.scroll_rows());
    assert!(scroll_rows > 0.0, "moving past the viewport must scroll the view, got {scroll_rows}");

    // The cursor stays inside the scrolloff-safe window: below the top
    // margin and above the bottom edge of the visible viewport.
    let top = scroll_rows as u64;
    assert!(
        (top + SCROLLOFF..top + visible_rows).contains(&cursor_row),
        "cursor row {cursor_row} should sit inside scrolloff window of viewport top {top} ({visible_rows} rows)"
    );
}
