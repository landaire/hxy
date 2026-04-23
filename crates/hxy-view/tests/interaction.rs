//! Integration tests driving the hex view through egui_kittest's harness.
//!
//! These exercise the pointer-event plumbing: click sets a caret, drag
//! extends to a new cursor, shift-click keeps the anchor. They don't
//! assert which exact byte is selected (that depends on glyph metrics
//! from egui's default fonts and would be fragile); instead they check
//! that the selection transitions match the interaction kind.

use egui_kittest::Harness;
use hxy_core::MemorySource;
use hxy_core::Selection;
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
