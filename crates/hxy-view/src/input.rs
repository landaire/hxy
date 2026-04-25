//! Keyboard event dispatch for [`crate::HexEditor`]. Drains egui
//! `Key` and `Text` events, applies navigation / selection updates,
//! and (with the `editor` feature) routes hex digits and ASCII
//! characters into the editor's write path.
//!
//! Consumers wire this up by calling [`crate::HexEditor::handle_input`]
//! once per frame, typically after the main panel has laid out any
//! widgets (text inputs, the command palette) that should have first
//! chance at keyboard focus.

use crate::HexEditor;
#[cfg(feature = "editor")]
use crate::Pane;
use hxy_core::ByteOffset;
use hxy_core::Selection;

/// Horizontal cursor step used by [`nav_nibble`]. A dedicated enum
/// (over a signed `i32` / `-1` / `+1` sentinel) keeps the call site
/// readable and prevents callers from passing nonsense magnitudes.
#[derive(Clone, Copy, Debug)]
pub(crate) enum HorizStep {
    Left,
    Right,
}

/// Vertical (row) cursor step used by [`nav_row`].
#[derive(Clone, Copy, Debug)]
pub(crate) enum VertStep {
    Up,
    Down,
}

/// Whether an arrow-key press should extend the existing selection
/// from its anchor or collapse to a fresh caret at the new cursor.
/// Shift determines which at the dispatcher; having a typed flag
/// keeps call sites explicit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Extend {
    /// Plain arrow: move anchor to follow cursor.
    No,
    /// Shift + arrow: keep anchor pinned, extend selection.
    Yes,
}

impl Extend {
    pub(crate) fn from_shift(shift: bool) -> Self {
        if shift { Extend::Yes } else { Extend::No }
    }
    fn extends(self) -> bool {
        matches!(self, Extend::Yes)
    }
}

#[derive(Debug)]
pub(crate) enum EditPress {
    #[cfg(feature = "editor")]
    Hex(u8),
    #[cfg(feature = "editor")]
    Ascii(u8),
    /// Move the cursor horizontally by one nibble (or one byte when
    /// the `editor` feature is off).
    NavHoriz(HorizStep, Extend),
    /// Move the cursor vertically by one row.
    NavVert(VertStep, Extend),
    /// Collapse the selection to a caret at the current cursor and
    /// reset any half-typed-nibble pointer. Bound to Escape.
    ClearSelection,
    /// Insert-mode Backspace: delete the byte before the cursor and
    /// step back. Only emitted when the editor's typing mode is
    /// `Insert` -- in `Replace` mode Backspace falls through.
    #[cfg(feature = "editor")]
    Backspace,
}

impl EditPress {
    fn is_navigation(&self) -> bool {
        matches!(self, EditPress::NavHoriz(..) | EditPress::NavVert(..))
    }
}

#[cfg(feature = "editor")]
pub(crate) fn key_to_hex_nibble(key: egui::Key) -> Option<u8> {
    use egui::Key as K;
    Some(match key {
        K::Num0 => 0,
        K::Num1 => 1,
        K::Num2 => 2,
        K::Num3 => 3,
        K::Num4 => 4,
        K::Num5 => 5,
        K::Num6 => 6,
        K::Num7 => 7,
        K::Num8 => 8,
        K::Num9 => 9,
        K::A => 0xA,
        K::B => 0xB,
        K::C => 0xC,
        K::D => 0xD,
        K::E => 0xE,
        K::F => 0xF,
        _ => return None,
    })
}

pub(crate) fn dispatch(editor: &mut HexEditor, ctx: &egui::Context) {
    if ctx.egui_wants_keyboard_input() {
        return;
    }

    // Detect cursor moves that came from outside this dispatcher
    // (mouse click, programmatic "jump to span") and reset the
    // nibble cursor so the next press lands on the high nibble of
    // the new byte. Arrow-key moves below update
    // `last_cursor_offset` themselves.
    let current_cursor = editor.selection.as_ref().map(|s| s.cursor.get());
    if current_cursor != editor.last_cursor_offset {
        #[cfg(feature = "editor")]
        editor.reset_edit_nibble();
        editor.push_history_boundary();
        editor.last_cursor_offset = current_cursor;
    }

    #[cfg(feature = "editor")]
    let mutable = editor.edit.mode == crate::editor::EditMode::Mutable;
    #[cfg(not(feature = "editor"))]
    let mutable = false;
    #[cfg(feature = "editor")]
    let inserting = editor.edit.typing_mode == crate::editor::TypingMode::Insert;
    let pane = editor.active_pane;

    let presses: Vec<EditPress> = ctx.input_mut(|i| {
        let mut out = Vec::new();
        i.events.retain(|event| match event {
            egui::Event::Key { key, pressed: true, modifiers, repeat: _, .. } => {
                if modifiers.command || modifiers.alt {
                    return true;
                }
                #[cfg(feature = "editor")]
                if mutable
                    && pane == Pane::Hex
                    && let Some(nibble) = key_to_hex_nibble(*key)
                {
                    out.push(EditPress::Hex(nibble));
                    return false;
                }
                // Without the editor feature `mutable`/`pane` are
                // unused in this branch; silence the warning.
                let _ = mutable;
                let _ = pane;
                let extend = Extend::from_shift(modifiers.shift);
                match key {
                    egui::Key::ArrowLeft => {
                        out.push(EditPress::NavHoriz(HorizStep::Left, extend));
                        false
                    }
                    egui::Key::ArrowRight => {
                        out.push(EditPress::NavHoriz(HorizStep::Right, extend));
                        false
                    }
                    egui::Key::ArrowUp => {
                        out.push(EditPress::NavVert(VertStep::Up, extend));
                        false
                    }
                    egui::Key::ArrowDown => {
                        out.push(EditPress::NavVert(VertStep::Down, extend));
                        false
                    }
                    egui::Key::Escape => {
                        out.push(EditPress::ClearSelection);
                        false
                    }
                    #[cfg(feature = "editor")]
                    egui::Key::Backspace if mutable && inserting => {
                        out.push(EditPress::Backspace);
                        false
                    }
                    _ => true,
                }
            }
            #[cfg(feature = "editor")]
            egui::Event::Text(s) if mutable && pane == Pane::Ascii => {
                let mut consumed = false;
                for ch in s.chars() {
                    if ch.is_ascii_graphic() || ch == ' ' {
                        out.push(EditPress::Ascii(ch as u8));
                        consumed = true;
                    }
                }
                !consumed
            }
            _ => true,
        });
        out
    });
    if presses.is_empty() {
        return;
    }

    let columns = editor.last_columns.map(|c| u64::from(c.get())).unwrap_or(16);
    let source_len = editor.source.len().get();
    if editor.selection.is_none() && presses.iter().any(EditPress::is_navigation) {
        editor.selection = Some(Selection::caret(ByteOffset::new(0)));
        #[cfg(feature = "editor")]
        editor.reset_edit_nibble();
    }

    for press in presses {
        match press {
            #[cfg(feature = "editor")]
            EditPress::Hex(nibble) => match editor.type_hex_digit(nibble) {
                Ok(true) => advance_cursor_byte(editor),
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %e, "hex edit"),
            },
            #[cfg(feature = "editor")]
            EditPress::Ascii(byte) => match editor.type_ascii_byte(byte) {
                Ok(true) => advance_cursor_byte(editor),
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %e, "ascii edit"),
            },
            EditPress::NavHoriz(step, extend) => {
                nav_nibble(editor, step, extend);
                editor.push_history_boundary();
            }
            EditPress::NavVert(step, extend) => {
                nav_row(editor, step, columns, source_len, extend);
                editor.push_history_boundary();
            }
            EditPress::ClearSelection => {
                if let Some(sel) = editor.selection.as_mut() {
                    sel.anchor = sel.cursor;
                }
                #[cfg(feature = "editor")]
                editor.reset_edit_nibble();
            }
            #[cfg(feature = "editor")]
            EditPress::Backspace => match editor.backspace_byte() {
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "backspace"),
            },
        }
    }
    editor.last_cursor_offset = editor.selection.as_ref().map(|s| s.cursor.get());
}

/// Advance the cursor by one whole byte (clamp at EOF). Collapses
/// any live selection to a caret -- typing isn't a selection-
/// extending op.
#[cfg(feature = "editor")]
pub(crate) fn advance_cursor_byte(editor: &mut HexEditor) {
    if let Some(sel) = editor.selection.as_mut() {
        let next = sel.cursor.get().saturating_add(1).min(editor.source.len().get());
        sel.cursor = ByteOffset::new(next);
        sel.anchor = sel.cursor;
    }
}

pub(crate) fn nav_nibble(editor: &mut HexEditor, step: HorizStep, extend: Extend) {
    // Nibble-granular stepping only makes sense in the hex pane
    // when editing -- in the ASCII pane each cell is exactly one
    // byte, and without the editor feature there's no nibble
    // pointer at all. In either of those cases arrow keys move a
    // whole byte.
    #[cfg(feature = "editor")]
    let nibble_granular = matches!(editor.active_pane, Pane::Hex);
    #[cfg(not(feature = "editor"))]
    let nibble_granular = false;

    let Some(sel) = editor.selection.as_mut() else { return };
    let source_len = editor.source.len().get();
    if nibble_granular {
        #[cfg(feature = "editor")]
        match step {
            HorizStep::Right => {
                if editor.edit.edit_high_nibble {
                    editor.edit.edit_high_nibble = false;
                } else {
                    let next = sel.cursor.get().saturating_add(1).min(source_len);
                    sel.cursor = ByteOffset::new(next);
                    editor.edit.edit_high_nibble = true;
                }
            }
            HorizStep::Left => {
                if !editor.edit.edit_high_nibble {
                    editor.edit.edit_high_nibble = true;
                } else {
                    let cur = sel.cursor.get();
                    if cur > 0 {
                        sel.cursor = ByteOffset::new(cur - 1);
                        editor.edit.edit_high_nibble = false;
                    }
                }
            }
        }
    } else {
        match step {
            HorizStep::Right => {
                let next = sel.cursor.get().saturating_add(1).min(source_len);
                sel.cursor = ByteOffset::new(next);
            }
            HorizStep::Left => {
                let cur = sel.cursor.get();
                if cur > 0 {
                    sel.cursor = ByteOffset::new(cur - 1);
                }
            }
        }
        // ASCII-pane moves land on a whole byte: reset any half-
        // typed-nibble state so flipping back to the hex pane
        // starts fresh on the high nibble.
        #[cfg(feature = "editor")]
        {
            editor.edit.edit_high_nibble = true;
        }
    }
    if !extend.extends() {
        sel.anchor = sel.cursor;
    }
}

pub(crate) fn nav_row(editor: &mut HexEditor, step: VertStep, columns: u64, source_len: u64, extend: Extend) {
    if columns == 0 {
        return;
    }
    let Some(sel) = editor.selection.as_mut() else { return };
    let cur = sel.cursor.get();
    let new = match step {
        VertStep::Down => {
            let candidate = cur.saturating_add(columns);
            let last = source_len.saturating_sub(1);
            candidate.min(last)
        }
        VertStep::Up => cur.saturating_sub(columns),
    };
    sel.cursor = ByteOffset::new(new);
    #[cfg(feature = "editor")]
    {
        editor.edit.edit_high_nibble = true;
    }
    if !extend.extends() {
        sel.anchor = sel.cursor;
    }
}
