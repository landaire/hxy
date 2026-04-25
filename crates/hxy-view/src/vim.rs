//! Vim-style modal input for [`crate::HexEditor`].
//!
//! Opt-in alternative to [`crate::input::dispatch`]; the editor's
//! [`InputMode`] selects between them. v1 ships the basics:
//!
//! * Modes: Normal, Visual (charwise), Visual-line, Insert.
//! * Motions: `h j k l` with optional count prefix (`5j`).
//! * Pane swap: `Tab` toggles hex / ASCII while in Normal.
//! * Insert: `i` enters Insert; `Esc` returns to Normal. Insert mode
//!   delegates back to [`crate::input::dispatch`] so the existing
//!   nibble-level edit machinery, ASCII typing, and arrow-key
//!   navigation all work unchanged.
//!
//! Word motions, visual-mode, yank/paste, delete, text objects, and
//! find-char land in subsequent stages -- the state machine here is
//! sized for those without rework.

use egui::Key;
use egui::Modifiers;
use hxy_core::ByteOffset;
use hxy_core::Selection;

use crate::HexEditor;
use crate::Pane;
use crate::input;

/// Top-level input style on the editor. Library consumers flip this
/// directly via [`HexEditor::set_input_mode`]; the host app layers a
/// user setting on top to opt entire sessions into Vim mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum InputMode {
    /// Standard arrow-key + typing dispatch, identical to the
    /// pre-vim behaviour.
    #[default]
    Default,
    /// Modal editing -- see module docs.
    Vim,
}

/// Which sub-mode Vim mode is currently in. Roughly matches vim's
/// `'mode'` variable; visual-block (`<C-v>`) is intentionally absent
/// for v1 since byte-level rectangular selection has subtle clamp
/// rules in a hex view and isn't worth doing badly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VimMode {
    #[default]
    Normal,
    /// Charwise visual: selection extends byte-by-byte as the user
    /// invokes motions. (Stage 3.)
    Visual,
    /// Linewise visual: selection snaps to whole rows in the hex view
    /// (or whole text-lines in the ASCII pane). (Stage 3.)
    VisualLine,
    /// Insert mode -- typing flows through the standard hex/ASCII
    /// editor dispatch. `Esc` returns to Normal.
    Insert,
}

/// Per-editor Vim state. Lives on `HexEditor` whether or not Vim
/// mode is active so toggling between modes mid-session doesn't
/// have to allocate a fresh state machine.
#[derive(Clone, Debug, Default)]
pub struct VimState {
    pub mode: VimMode,
    /// Numeric prefix the user has typed (e.g. the `5` in `5j`).
    /// Cleared after the next motion / operator consumes it.
    pub count: Option<usize>,
}

impl VimState {
    /// `count` defaulting to 1 for motions / operators that haven't
    /// been preceded by a digit prefix.
    pub fn count_or_one(&self) -> usize {
        self.count.unwrap_or(1)
    }
    pub fn clear_pending(&mut self) {
        self.count = None;
    }
}

/// Drive one frame of Vim input. Called only when the editor's
/// [`InputMode`] is `Vim`; otherwise the standard
/// [`crate::input::dispatch`] runs.
pub(crate) fn dispatch(editor: &mut HexEditor, ctx: &egui::Context) {
    if ctx.egui_wants_keyboard_input() {
        return;
    }

    // Insert mode delegates entirely to the standard dispatcher --
    // hex digits, ASCII typing, arrow keys, copy/paste, selection
    // clearing, the lot. We just intercept Esc first to pop back to
    // Normal so the editor's own Esc handler doesn't also clear the
    // selection on the same press.
    if editor.vim.mode == VimMode::Insert {
        let escape = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
        if escape {
            editor.vim.mode = VimMode::Normal;
            #[cfg(feature = "editor")]
            editor.reset_edit_nibble();
            return;
        }
        input::dispatch(editor, ctx);
        return;
    }

    // Drain Key events ourselves so Vim's bindings don't compete
    // with the standard dispatcher's handling. The default
    // dispatcher's first action is to early-return on
    // `egui_wants_keyboard_input`, which we already checked, so
    // there's no conflict during Normal/Visual.
    let columns = editor.last_columns.map(|c| u64::from(c.get())).unwrap_or(16);
    let source_len = editor.source.len().get();

    let presses: Vec<VimPress> = ctx.input_mut(|i| {
        let mut out = Vec::new();
        i.events.retain(|event| match event {
            egui::Event::Key { key, pressed: true, modifiers, repeat: _, .. } => {
                if modifiers.command || modifiers.alt {
                    return true;
                }
                match key {
                    Key::Escape => {
                        out.push(VimPress::Escape);
                        false
                    }
                    Key::Tab => {
                        out.push(VimPress::TogglePane);
                        false
                    }
                    Key::H => {
                        out.push(VimPress::Motion(Motion::Left));
                        false
                    }
                    Key::J => {
                        out.push(VimPress::Motion(Motion::Down));
                        false
                    }
                    Key::K => {
                        out.push(VimPress::Motion(Motion::Up));
                        false
                    }
                    Key::L => {
                        out.push(VimPress::Motion(Motion::Right));
                        false
                    }
                    Key::I => {
                        out.push(VimPress::EnterInsert);
                        false
                    }
                    Key::Num0 if !modifiers.shift => {
                        // Pure 0 with no count buffer is the LineStart
                        // motion; with a count buffer it's a digit.
                        // We resolve that in the apply step below; the
                        // press carries the digit value and apply
                        // checks the count buffer.
                        out.push(VimPress::Digit(0));
                        false
                    }
                    Key::Num1 => {
                        out.push(VimPress::Digit(1));
                        false
                    }
                    Key::Num2 => {
                        out.push(VimPress::Digit(2));
                        false
                    }
                    Key::Num3 => {
                        out.push(VimPress::Digit(3));
                        false
                    }
                    Key::Num4 => {
                        out.push(VimPress::Digit(4));
                        false
                    }
                    Key::Num5 => {
                        out.push(VimPress::Digit(5));
                        false
                    }
                    Key::Num6 => {
                        out.push(VimPress::Digit(6));
                        false
                    }
                    Key::Num7 => {
                        out.push(VimPress::Digit(7));
                        false
                    }
                    Key::Num8 => {
                        out.push(VimPress::Digit(8));
                        false
                    }
                    Key::Num9 => {
                        out.push(VimPress::Digit(9));
                        false
                    }
                    _ => true,
                }
            }
            // Swallow Text events too -- otherwise typing letters
            // that happen to also be motions (h/j/k/l) leaks an
            // Event::Text for some integrations and lands a literal
            // 'h' in any focused TextEdit.
            egui::Event::Text(_) => false,
            _ => true,
        });
        out
    });

    if presses.is_empty() {
        return;
    }

    if editor.selection.is_none() {
        editor.selection = Some(Selection::caret(ByteOffset::new(0)));
    }

    for press in presses {
        match press {
            VimPress::Escape => {
                editor.vim.clear_pending();
                if let Some(sel) = editor.selection.as_mut() {
                    sel.anchor = sel.cursor;
                }
                #[cfg(feature = "editor")]
                editor.reset_edit_nibble();
            }
            VimPress::TogglePane => {
                let other = match editor.active_pane {
                    Pane::Hex => Pane::Ascii,
                    Pane::Ascii => Pane::Hex,
                };
                editor.set_active_pane(other);
                editor.vim.clear_pending();
            }
            VimPress::EnterInsert => {
                editor.vim.mode = VimMode::Insert;
                editor.vim.clear_pending();
            }
            VimPress::Digit(d) => {
                // Pure 0 with no buffered count is "go to line start"
                // -- reserved for a later motion; for v1 it's a no-op
                // so we don't accidentally consume the keypress in a
                // way the user can't predict.
                if d == 0 && editor.vim.count.is_none() {
                    continue;
                }
                let prev = editor.vim.count.unwrap_or(0);
                let next = prev.saturating_mul(10).saturating_add(d as usize);
                editor.vim.count = Some(next);
            }
            VimPress::Motion(motion) => {
                let count = editor.vim.count_or_one();
                editor.vim.clear_pending();
                apply_motion(editor, motion, count, columns, source_len);
            }
        }
    }

    editor.last_cursor_offset = editor.selection.as_ref().map(|s| s.cursor.get());
}

#[derive(Clone, Copy, Debug)]
enum VimPress {
    Escape,
    TogglePane,
    EnterInsert,
    Digit(u8),
    Motion(Motion),
}

#[derive(Clone, Copy, Debug)]
enum Motion {
    Left,
    Right,
    Up,
    Down,
}

fn apply_motion(editor: &mut HexEditor, motion: Motion, count: usize, columns: u64, source_len: u64) {
    let extending = matches!(editor.vim.mode, VimMode::Visual | VimMode::VisualLine);
    let extend = if extending { input::Extend::Yes } else { input::Extend::No };
    for _ in 0..count {
        match motion {
            Motion::Left => input::nav_nibble(editor, input::HorizStep::Left, extend),
            Motion::Right => input::nav_nibble(editor, input::HorizStep::Right, extend),
            Motion::Up => input::nav_row(editor, input::VertStep::Up, columns, source_len, extend),
            Motion::Down => input::nav_row(editor, input::VertStep::Down, columns, source_len, extend),
        }
    }
}
