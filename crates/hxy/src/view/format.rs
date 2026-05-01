//! Tiny formatting + clipboard helpers shared across the status
//! bar, hex view, and palette context-builders.

pub fn format_offset(value: u64, base: crate::settings::OffsetBase) -> String {
    match base {
        crate::settings::NumericBase::Hex => format!("0x{value:X}"),
        crate::settings::NumericBase::Decimal => format!("{value}"),
    }
}

/// Format `value` as a virtual address: shifts by `vaddr` before
/// rendering with `base`. Saturates on overflow rather than wrapping
/// so an absurd vaddr can't silently produce a tiny address.
pub fn format_offset_with_vaddr(value: u64, base: crate::settings::OffsetBase, vaddr: u64) -> String {
    format_offset(value.saturating_add(vaddr), base)
}

/// Format a byte offset / length / end position using the
/// user's configured [`NumericFormat`]. Routes through
/// [`format_offset`] after picking the right base for `value`,
/// so callers don't have to write the threshold check
/// themselves. Use this everywhere a numeric span value is
/// rendered outside the status bar.
pub fn format_numeric(value: u64, fmt: crate::settings::NumericFormat) -> String {
    format_offset(value, fmt.pick(value))
}

/// Click to toggle offset base, hover for the alternate-base tooltip,
/// and -- while hovered -- consume Cmd/Ctrl+C to copy the label's text.
/// Consuming the shortcut keeps the hex-view selection copy handler
/// from also firing in the same frame.
pub fn copyable_status_label(
    ui: &mut egui::Ui,
    display: &str,
    copy: &str,
    tooltip: Option<String>,
    new_base: &mut crate::settings::OffsetBase,
    base: crate::settings::OffsetBase,
) {
    let r = ui.add(egui::Label::new(display).sense(egui::Sense::click()));
    if r.clicked() {
        *new_base = base.toggle();
    }
    // Direct pointer-in-rect check: `r.hovered()` and even
    // `ui.rect_contains_pointer` can read false when a tooltip or
    // neighbouring widget counts as covering the label. Reading the
    // pointer position and testing `r.rect.contains(p)` bypasses
    // egui's widget-layering bookkeeping entirely -- which is what
    // we want for a whole-cell-is-the-target hover.
    let over_label = ui.ctx().input(|i| i.pointer.latest_pos()).is_some_and(|p| r.rect.contains(p));
    let r = if let Some(tt) = tooltip { r.on_hover_text(tt) } else { r };
    let _ = r;
    if over_label && ui.ctx().input_mut(consume_copy_event_local) {
        ui.ctx().copy_text(copy.to_string());
    }
}

/// Drain the user's "copy" gesture in any form egui delivers it:
/// the semantic `Event::Copy` (winit on macOS converts Cmd+C to
/// this) and the raw `Cmd/Ctrl+C` keypress. Drains both because
/// macOS sends both for the same gesture and the hex view's
/// dispatcher would grab whichever survives. Mirrors the desktop
/// app's `crate::app::shortcuts::consume_copy_event` so it works
/// on every target (this module compiles on wasm too).
fn consume_copy_event_local(input: &mut egui::InputState) -> bool {
    let copy_shortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::C);
    let mut any = false;
    let before = input.events.len();
    input.events.retain(|e| !matches!(e, egui::Event::Copy));
    if input.events.len() != before {
        any = true;
    }
    if input.consume_shortcut(&copy_shortcut) {
        any = true;
    }
    any
}
