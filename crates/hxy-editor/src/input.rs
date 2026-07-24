//! Default (non-modal) keyboard dispatch for [`crate::HexEditor`].
//! Applies navigation / selection updates and (with the `editor`
//! feature) routes hex digits and ASCII characters into the editor's
//! write path.
//!
//! Event translation lives in [`crate::dispatch::InputFilter`]; this
//! module owns the per-frame apply logic that consumes the resulting
//! [`EditPress`] batch.

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

/// Apply one frame's default-dispatch presses. Runs the frame-begin
/// bookkeeping (external-cursor-move detection) even for an empty
/// batch, then the presses and frame-end scrolloff.
pub(crate) fn apply(editor: &mut HexEditor, presses: Vec<EditPress>) {
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
    // Snapshot the cursor at the start of input processing so the
    // post-dispatch scrolloff check can detect "did this dispatch move
    // the cursor". We compare against the post-dispatch cursor below.
    let cursor_before_dispatch = current_cursor;

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
    let cursor_after_dispatch = editor.selection.as_ref().map(|s| s.cursor.get());
    if cursor_after_dispatch.is_some() && cursor_after_dispatch != cursor_before_dispatch {
        editor.ensure_cursor_visible_with_scrolloff(SCROLLOFF_ROWS);
    }
    editor.last_cursor_offset = cursor_after_dispatch;
}

/// Rows of context kept above and below the cursor when a dispatcher
/// scrolls to follow it. Mirrors vim's default `'scrolloff'`.
pub const SCROLLOFF_ROWS: u64 = 3;

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
