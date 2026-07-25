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
