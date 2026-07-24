use std::sync::Arc;

use hxy_core::ByteOffset;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_core::Selection;
use hxy_editor::Disposition;
use hxy_editor::Effect;
use hxy_editor::HexEditor;
use hxy_editor::InputEvent;
use hxy_editor::InputMode;
use hxy_editor::Key;
use hxy_editor::Modifiers;
use hxy_editor::Pane;
use hxy_editor::VimMode;

fn editor(bytes: &[u8]) -> HexEditor {
    let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes.to_vec()));
    let mut ed = HexEditor::new(source);
    ed.set_selection(Some(Selection::caret(ByteOffset::new(0))));
    ed
}

fn key(k: Key) -> InputEvent {
    InputEvent::Key { key: k, modifiers: Modifiers::default() }
}

fn shift(k: Key) -> InputEvent {
    InputEvent::Key { key: k, modifiers: Modifiers { shift: true, ..Modifiers::default() } }
}

fn feed_all(ed: &mut HexEditor, events: &[InputEvent]) -> Vec<Effect> {
    let mut filter = ed.input_filter();
    for e in events {
        filter.feed(e);
    }
    ed.apply_input(filter.finish())
}

#[test]
fn typing_two_hex_digits_writes_byte_and_advances() {
    let mut ed = editor(&[0x00, 0x11]);
    feed_all(&mut ed, &[key(Key::Letter('a')), key(Key::Letter('b'))]);
    let range = hxy_core::ByteRange::new(ByteOffset::new(0), ByteOffset::new(1)).unwrap();
    assert_eq!(ed.source().read(range).unwrap()[0], 0xAB);
    assert_eq!(ed.selection().unwrap().cursor.get(), 1);
}

#[test]
fn arrow_right_with_shift_extends_selection() {
    let mut ed = editor(&[0x00, 0x11, 0x22]);
    ed.set_active_pane(Pane::Ascii);
    feed_all(&mut ed, &[shift(Key::ArrowRight), shift(Key::ArrowRight)]);
    let sel = ed.selection().unwrap();
    assert_eq!(sel.anchor.get(), 0);
    assert_eq!(sel.cursor.get(), 2);
}

#[test]
fn ascii_pane_text_event_types_byte() {
    let mut ed = editor(&[0x00]);
    ed.set_active_pane(Pane::Ascii);
    feed_all(&mut ed, &[InputEvent::Text("Z".to_owned())]);
    let range = hxy_core::ByteRange::new(ByteOffset::new(0), ByteOffset::new(1)).unwrap();
    assert_eq!(ed.source().read(range).unwrap()[0], b'Z');
}

#[test]
fn command_modifier_passes_through() {
    let mut ed = editor(&[0x00]);
    let mut filter = ed.input_filter();
    let ev = InputEvent::Key {
        key: Key::Letter('a'),
        modifiers: Modifiers { command: true, ..Modifiers::default() },
    };
    assert_eq!(filter.feed(&ev), Disposition::Passed);
}

#[test]
fn vim_motion_with_count() {
    let mut ed = editor(&[0u8; 64]);
    ed.set_input_mode(InputMode::Vim);
    // 2j = down two rows (16 columns default) -> offset 32.
    feed_all(&mut ed, &[key(Key::Digit(2)), key(Key::Letter('j'))]);
    assert_eq!(ed.selection().unwrap().cursor.get(), 32);
}

#[test]
fn vim_yank_row_emits_copy_effect() {
    let mut ed = editor(b"0123456789abcdef");
    ed.set_input_mode(InputMode::Vim);
    let effects = feed_all(&mut ed, &[key(Key::Letter('y')), key(Key::Letter('y'))]);
    assert_eq!(effects.len(), 1);
    let Effect::CopyText(text) = &effects[0];
    assert!(text.starts_with("30 31 32"), "hex-formatted yank, got: {text}");
}

#[test]
fn vim_insert_escape_swallows_batch_and_pops_mode() {
    let mut ed = editor(&[0x00]);
    ed.set_input_mode(InputMode::Vim);
    feed_all(&mut ed, &[key(Key::Letter('i'))]);
    assert_eq!(ed.vim_state().mode, VimMode::Insert);
    // Escape plus a trailing 'a' in one batch: mode pops, 'a' is NOT typed.
    feed_all(&mut ed, &[key(Key::Escape), key(Key::Letter('a'))]);
    assert_eq!(ed.vim_state().mode, VimMode::Normal);
    let range = hxy_core::ByteRange::new(ByteOffset::new(0), ByteOffset::new(1)).unwrap();
    assert_eq!(ed.source().read(range).unwrap()[0], 0x00);
}

#[test]
fn empty_batch_still_runs_frame_bookkeeping() {
    let mut ed = editor(&[0x00, 0x11]);
    // Simulate an external cursor move (mouse click), then an empty
    // input frame: the nibble cursor must reset to high.
    feed_all(&mut ed, &[key(Key::Letter('a'))]); // half-typed byte, nibble now low
    ed.set_selection(Some(Selection::caret(ByteOffset::new(1))));
    feed_all(&mut ed, &[]);
    // Typing one digit now must hit the HIGH nibble of byte 1.
    feed_all(&mut ed, &[key(Key::Letter('c'))]);
    let range = hxy_core::ByteRange::new(ByteOffset::new(1), ByteOffset::new(2)).unwrap();
    assert_eq!(ed.source().read(range).unwrap()[0], 0xC1);
}
