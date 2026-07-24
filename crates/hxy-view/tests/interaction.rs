//! Integration tests driving the hex view through egui_kittest's harness.
//!
//! These exercise the pointer-event plumbing: click sets a caret, drag
//! extends to a new cursor, shift-click keeps the anchor. They don't
//! assert which exact byte is selected (that depends on glyph metrics
//! from egui's default fonts and would be fragile); instead they check
//! that the selection transitions match the interaction kind.

use std::sync::Arc;

use egui_kittest::Harness;
use hxy_core::ByteOffset;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_core::Selection;
use hxy_view::HexEditor;
use hxy_view::HexEditorExt;
use hxy_view::HexView;

struct TestState {
    source: MemorySource,
    selection: Option<Selection>,
}

fn sample_state() -> TestState {
    TestState { source: MemorySource::new((0u8..=255).cycle().take(1024).collect::<Vec<_>>()), selection: None }
}

fn build_harness<'a>(state: TestState) -> Harness<'a, TestState> {
    Harness::builder().with_size(egui::Vec2::new(800.0, 600.0)).with_pixels_per_point(1.0).build_ui_state(
        |ui, st: &mut TestState| {
            HexView::new(&st.source, &mut st.selection).show(ui);
        },
        state,
    )
}

/// Push a key press (down + up) and run a frame after each so the
/// adapter sees both edges, mirroring `click_at`'s two-frame shape.
fn press_key(harness: &mut Harness<'_, HexEditor>, key: egui::Key) {
    harness.input_mut().events.push(egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::default(),
    });
    harness.run();
    harness.input_mut().events.push(egui::Event::Key {
        key,
        physical_key: None,
        pressed: false,
        repeat: false,
        modifiers: egui::Modifiers::default(),
    });
    harness.run();
}

/// Push a simple click event at `pos` and run two frames so egui sees
/// both the press and release.
fn click_at(harness: &mut Harness<'_, TestState>, pos: egui::Pos2) {
    harness.input_mut().events.push(egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::default(),
    });
    harness.input_mut().events.push(egui::Event::PointerMoved(pos));
    harness.run();
    harness.input_mut().events.push(egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::default(),
    });
    harness.run();
}

#[test]
fn click_inside_hex_pane_sets_caret() {
    let mut harness = build_harness(sample_state());
    harness.run();

    // Middle of the viewport is reliably inside the hex pane (with 16
    // columns of ~24px each at 1 ppp).
    click_at(&mut harness, egui::Pos2::new(200.0, 100.0));

    let sel = harness.state().selection.expect("expected selection after click");
    assert!(sel.is_caret(), "click should produce a caret, got {sel:?}");
}

#[test]
fn drag_creates_nonempty_selection() {
    let mut harness = build_harness(sample_state());
    harness.run();

    let start = egui::Pos2::new(180.0, 50.0);
    let end = egui::Pos2::new(400.0, 200.0);

    harness.input_mut().events.push(egui::Event::PointerMoved(start));
    harness.input_mut().events.push(egui::Event::PointerButton {
        pos: start,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::default(),
    });
    harness.run();

    for step in 1..=4 {
        let t = step as f32 / 4.0;
        let pos = egui::Pos2::new(start.x + (end.x - start.x) * t, start.y + (end.y - start.y) * t);
        harness.input_mut().events.push(egui::Event::PointerMoved(pos));
        harness.run();
    }

    harness.input_mut().events.push(egui::Event::PointerButton {
        pos: end,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::default(),
    });
    harness.run();

    let sel = harness.state().selection.expect("expected selection after drag");
    assert!(!sel.is_caret(), "drag should produce a range, got a caret at {sel:?}");
    assert!(sel.anchor != sel.cursor, "anchor and cursor should differ after drag");
}

/// Drives the keyboard path: `handle_input` translates real egui key
/// events into `hxy-editor`'s `InputEvent`s, which the dispatcher
/// turns into hex-digit writes. Pointer tests above never exercise
/// this route.
#[test]
fn typing_hex_digits_writes_byte_through_adapter() {
    let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(vec![0x00u8; 16]));
    let mut editor = HexEditor::new(source);
    editor.set_selection(Some(Selection::caret(ByteOffset::new(0))));

    let mut harness: Harness<'_, HexEditor> =
        Harness::builder().with_size(egui::Vec2::new(800.0, 600.0)).with_pixels_per_point(1.0).build_ui_state(
            |ui, editor: &mut HexEditor| {
                editor.handle_input(ui.ctx());
                let response = editor.view().show(ui);
                editor.on_response(&response, hxy_core::ColumnCount::DEFAULT);
            },
            editor,
        );
    harness.run();

    press_key(&mut harness, egui::Key::A);
    press_key(&mut harness, egui::Key::B);

    let ed = harness.state();
    let range = hxy_core::ByteRange::new(ByteOffset::new(0), ByteOffset::new(1)).unwrap();
    assert_eq!(ed.source().read(range).unwrap()[0], 0xAB, "expected offset 0 to become 0xAB");
    assert_eq!(ed.selection().expect("caret after typing").cursor.get(), 1, "caret should advance to offset 1");
}
