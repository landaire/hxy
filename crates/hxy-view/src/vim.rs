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
    /// Multi-key prefix the user is partway through (e.g. the
    /// first `g` of `gg`). Cleared on completion or on `Esc`.
    pub pending: Option<Pending>,
    /// Vim's unnamed register: bytes from the last yank or delete.
    /// Used by `p` to paste at the cursor. Stays alive across mode
    /// switches and across files (lives on each editor; could
    /// become a host-level shared register later).
    pub register: Vec<u8>,
    /// Which pane the bytes in `register` were yanked from. Used to
    /// pick a clipboard format on yank: hex pane yanks render as
    /// space-separated hex, ASCII pane yanks as utf-8 lossy text.
    pub register_origin: RegisterOrigin,
}

/// Format hint attached to the unnamed register's contents. Doesn't
/// affect paste (we always paste raw bytes); only governs how yank
/// shapes the system-clipboard payload.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RegisterOrigin {
    #[default]
    Hex,
    Ascii,
}

/// In-progress multi-key sequence. Resolved by the next keypress
/// or cleared by `Esc`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pending {
    /// First `g` of a `g`-prefixed motion (`gg`, `gu`, ...).
    G,
    /// First `y` of a `yy` (linewise yank). Stage 3 keeps the
    /// operator-pending state minimal -- only the doubled form is
    /// recognised; arbitrary `y{motion}` lands in Stage 4.
    Yank,
    /// First `d` of a `dd` (linewise delete).
    Delete,
}

impl VimState {
    /// `count` defaulting to 1 for motions / operators that haven't
    /// been preceded by a digit prefix.
    pub fn count_or_one(&self) -> usize {
        self.count.unwrap_or(1)
    }
    pub fn clear_pending(&mut self) {
        self.count = None;
        self.pending = None;
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
                let shift = modifiers.shift;
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
                    Key::W => {
                        out.push(VimPress::Motion(if shift { Motion::WordEndForwardBig } else { Motion::WordForward }));
                        false
                    }
                    Key::B => {
                        out.push(VimPress::Motion(if shift { Motion::WordBackBig } else { Motion::WordBack }));
                        false
                    }
                    Key::E => {
                        out.push(VimPress::Motion(if shift { Motion::WordEndForwardBig } else { Motion::WordEndForward }));
                        false
                    }
                    Key::I => {
                        out.push(VimPress::EnterInsert);
                        false
                    }
                    Key::V => {
                        out.push(if shift { VimPress::EnterVisualLine } else { VimPress::EnterVisual });
                        false
                    }
                    Key::Y => {
                        out.push(VimPress::Yank);
                        false
                    }
                    Key::P => {
                        out.push(VimPress::Paste);
                        false
                    }
                    Key::D => {
                        out.push(VimPress::Delete);
                        false
                    }
                    Key::X => {
                        out.push(VimPress::DeleteByte);
                        false
                    }
                    Key::G => {
                        // Capital G = end-of-file; lowercase g
                        // starts a `gg` sequence (resolved on the
                        // next keypress).
                        out.push(if shift { VimPress::Motion(Motion::EndOfFile) } else { VimPress::PendingG });
                        false
                    }
                    // `$` (Shift+4) -- end of line / row. Must be
                    // before the bare `Num4 -> Digit(4)` arm,
                    // otherwise the unguarded arm wins.
                    Key::Num4 if shift => {
                        out.push(VimPress::Motion(Motion::LineEnd));
                        false
                    }
                    Key::Num0 if !shift => {
                        // Pure `0` with no count buffer is the line-
                        // start motion; with a count buffer it's a
                        // digit. The apply step inspects the count
                        // buffer and routes accordingly.
                        out.push(VimPress::Digit(0));
                        false
                    }
                    Key::Num1 if !shift => {
                        out.push(VimPress::Digit(1));
                        false
                    }
                    Key::Num2 if !shift => {
                        out.push(VimPress::Digit(2));
                        false
                    }
                    Key::Num3 if !shift => {
                        out.push(VimPress::Digit(3));
                        false
                    }
                    Key::Num4 if !shift => {
                        out.push(VimPress::Digit(4));
                        false
                    }
                    Key::Num5 if !shift => {
                        out.push(VimPress::Digit(5));
                        false
                    }
                    Key::Num6 if !shift => {
                        out.push(VimPress::Digit(6));
                        false
                    }
                    Key::Num7 if !shift => {
                        out.push(VimPress::Digit(7));
                        false
                    }
                    Key::Num8 if !shift => {
                        out.push(VimPress::Digit(8));
                        false
                    }
                    Key::Num9 if !shift => {
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
                if d == 0 && editor.vim.count.is_none() {
                    // Pure `0` with no count buffer = line-start
                    // motion. With a count buffer it's a digit.
                    apply_motion(editor, Motion::LineStart, 1, columns, source_len);
                    continue;
                }
                let prev = editor.vim.count.unwrap_or(0);
                let next = prev.saturating_mul(10).saturating_add(d as usize);
                editor.vim.count = Some(next);
            }
            VimPress::PendingG => {
                if editor.vim.pending == Some(Pending::G) {
                    // Second `g` -- run `gg` (start-of-file). vim
                    // normally honours the count as "go to line N";
                    // for hex view we treat the count as "go to row
                    // N", which means count * columns.
                    let count = editor.vim.count;
                    editor.vim.clear_pending();
                    let target = match count {
                        Some(n) => n.saturating_mul(columns as usize) as u64,
                        None => 0,
                    };
                    set_cursor(editor, target.min(source_len.saturating_sub(1)));
                } else {
                    editor.vim.pending = Some(Pending::G);
                }
            }
            VimPress::Motion(motion) => {
                let count = editor.vim.count_or_one();
                editor.vim.clear_pending();
                apply_motion(editor, motion, count, columns, source_len);
            }
            VimPress::EnterVisual => {
                editor.vim.clear_pending();
                editor.vim.mode = match editor.vim.mode {
                    VimMode::Visual => VimMode::Normal,
                    _ => VimMode::Visual,
                };
                ensure_anchor(editor);
            }
            VimPress::EnterVisualLine => {
                editor.vim.clear_pending();
                editor.vim.mode = match editor.vim.mode {
                    VimMode::VisualLine => VimMode::Normal,
                    _ => VimMode::VisualLine,
                };
                if editor.vim.mode == VimMode::VisualLine {
                    snap_visual_line(editor, columns, source_len);
                } else {
                    ensure_anchor(editor);
                }
            }
            VimPress::Yank => match editor.vim.mode {
                VimMode::Visual | VimMode::VisualLine => {
                    yank_selection(editor, ctx);
                    editor.vim.mode = VimMode::Normal;
                    editor.vim.clear_pending();
                }
                _ => {
                    if editor.vim.pending == Some(Pending::Yank) {
                        let count = editor.vim.count_or_one();
                        editor.vim.clear_pending();
                        yank_rows(editor, ctx, count, columns, source_len);
                    } else {
                        editor.vim.pending = Some(Pending::Yank);
                    }
                }
            },
            VimPress::Delete => match editor.vim.mode {
                VimMode::Visual | VimMode::VisualLine => {
                    delete_selection(editor, ctx);
                    editor.vim.mode = VimMode::Normal;
                    editor.vim.clear_pending();
                }
                _ => {
                    if editor.vim.pending == Some(Pending::Delete) {
                        let count = editor.vim.count_or_one();
                        editor.vim.clear_pending();
                        delete_rows(editor, ctx, count, columns, source_len);
                    } else {
                        editor.vim.pending = Some(Pending::Delete);
                    }
                }
            },
            VimPress::DeleteByte => {
                let count = editor.vim.count_or_one();
                editor.vim.clear_pending();
                delete_byte_under_cursor(editor, ctx, count);
            }
            VimPress::Paste => {
                let count = editor.vim.count_or_one();
                editor.vim.clear_pending();
                paste_register(editor, count);
            }
        }
    }

    editor.last_cursor_offset = editor.selection.as_ref().map(|s| s.cursor.get());
}

fn set_cursor(editor: &mut HexEditor, offset: u64) {
    let extending = matches!(editor.vim.mode, VimMode::Visual | VimMode::VisualLine);
    if let Some(sel) = editor.selection.as_mut() {
        sel.cursor = ByteOffset::new(offset);
        if !extending {
            sel.anchor = sel.cursor;
        }
    } else {
        editor.selection = Some(Selection::caret(ByteOffset::new(offset)));
    }
    #[cfg(feature = "editor")]
    {
        editor.edit.edit_high_nibble = true;
    }
}

#[derive(Clone, Copy, Debug)]
enum VimPress {
    Escape,
    TogglePane,
    EnterInsert,
    EnterVisual,
    EnterVisualLine,
    Digit(u8),
    /// First `g` of a multi-key sequence (`gg` etc.). Resolved on
    /// the next press.
    PendingG,
    /// `y` -- yank. In Visual: yanks the live selection. In Normal:
    /// pressing twice (`yy`) yanks the current row.
    Yank,
    /// `p` -- paste the unnamed register at / after the cursor.
    Paste,
    /// `d` -- delete (shifts bytes left). Visual: deletes the live
    /// selection; Normal: pressing twice (`dd`) deletes the row.
    Delete,
    /// `x` -- delete the byte under the cursor.
    DeleteByte,
    Motion(Motion),
}

#[derive(Clone, Copy, Debug)]
enum Motion {
    Left,
    Right,
    Up,
    Down,
    /// `w` -- forward to start of next word. ASCII pane uses
    /// alphanumeric+underscore "word" semantics; hex pane steps
    /// one byte (each byte is its own "word").
    WordForward,
    /// `b` -- backward to start of word.
    WordBack,
    /// `e` -- forward to end of current/next word.
    WordEndForward,
    /// `W` / `E` -- whitespace-separated WORDs (ASCII pane);
    /// behaves like `w`/`e` in the hex pane.
    WordEndForwardBig,
    /// `B`.
    WordBackBig,
    /// `0` -- start of current row (hex) or line (ASCII).
    LineStart,
    /// `$` -- last byte of current row (hex) or last byte before
    /// the next newline (ASCII, EOF if no newline ahead).
    LineEnd,
    /// `G` -- last byte of file.
    EndOfFile,
}

fn apply_motion(editor: &mut HexEditor, motion: Motion, count: usize, columns: u64, source_len: u64) {
    let extending = matches!(editor.vim.mode, VimMode::Visual | VimMode::VisualLine);
    let extend = if extending { input::Extend::Yes } else { input::Extend::No };
    match motion {
        Motion::Left | Motion::Right | Motion::Up | Motion::Down => {
            for _ in 0..count {
                match motion {
                    Motion::Left => input::nav_nibble(editor, input::HorizStep::Left, extend),
                    Motion::Right => input::nav_nibble(editor, input::HorizStep::Right, extend),
                    Motion::Up => input::nav_row(editor, input::VertStep::Up, columns, source_len, extend),
                    Motion::Down => input::nav_row(editor, input::VertStep::Down, columns, source_len, extend),
                    _ => unreachable!(),
                }
            }
        }
        Motion::EndOfFile => {
            let target = source_len.saturating_sub(1);
            set_cursor(editor, target);
        }
        Motion::LineStart => {
            let cur = current_cursor(editor).unwrap_or(0);
            let target = match editor.active_pane {
                Pane::Hex => (cur / columns) * columns,
                Pane::Ascii => find_line_start(editor, cur),
            };
            set_cursor(editor, target.min(source_len.saturating_sub(1)));
        }
        Motion::LineEnd => {
            let cur = current_cursor(editor).unwrap_or(0);
            let target = match editor.active_pane {
                Pane::Hex => {
                    let row_end = (cur / columns) * columns + columns.saturating_sub(1);
                    row_end.min(source_len.saturating_sub(1))
                }
                Pane::Ascii => find_line_end(editor, cur, source_len),
            };
            set_cursor(editor, target);
        }
        Motion::WordForward
        | Motion::WordBack
        | Motion::WordEndForward
        | Motion::WordEndForwardBig
        | Motion::WordBackBig => {
            let mut cur = current_cursor(editor).unwrap_or(0);
            for _ in 0..count {
                cur = match motion {
                    Motion::WordForward => word_forward(editor, cur, source_len, false),
                    Motion::WordBack => word_back(editor, cur, false),
                    Motion::WordEndForward => word_end_forward(editor, cur, source_len, false),
                    Motion::WordEndForwardBig => word_end_forward(editor, cur, source_len, true),
                    Motion::WordBackBig => word_back(editor, cur, true),
                    _ => unreachable!(),
                };
            }
            set_cursor(editor, cur);
        }
    }
}

fn current_cursor(editor: &HexEditor) -> Option<u64> {
    editor.selection.as_ref().map(|s| s.cursor.get())
}

/// Read one byte at `offset`. Returns `None` past EOF or on read
/// error -- callers treat that as a non-word boundary.
fn byte_at(editor: &HexEditor, offset: u64) -> Option<u8> {
    use hxy_core::ByteRange;
    let range = ByteRange::new(ByteOffset::new(offset), ByteOffset::new(offset + 1)).ok()?;
    editor.source.read(range).ok()?.first().copied()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Word,
    Punct,
    Whitespace,
}

/// Vim's "small word" classification: alphanumeric + underscore =
/// Word, whitespace = Whitespace, everything else = Punct. `big`
/// folds Word + Punct into Word so `W`/`B`/`E` see whitespace as
/// the only separator.
fn classify(b: u8, big: bool) -> CharClass {
    if b.is_ascii_whitespace() {
        CharClass::Whitespace
    } else if big || b.is_ascii_alphanumeric() || b == b'_' {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}

/// In the hex pane every byte is a "word" -- step by one. In the
/// ASCII pane walk forward to the start of the next non-whitespace
/// thing across the appropriate class boundary.
fn word_forward(editor: &HexEditor, from: u64, source_len: u64, big: bool) -> u64 {
    if matches!(editor.active_pane, Pane::Hex) {
        return (from + 1).min(source_len.saturating_sub(1));
    }
    let starting_class = byte_at(editor, from).map(|b| classify(b, big));
    let mut i = from + 1;
    // Skip the rest of the current class.
    while i < source_len {
        let class = byte_at(editor, i).map(|b| classify(b, big));
        if class != starting_class {
            break;
        }
        i += 1;
    }
    // Skip whitespace to land on the next non-whitespace start.
    while i < source_len && byte_at(editor, i).is_some_and(|b| b.is_ascii_whitespace()) {
        i += 1;
    }
    i.min(source_len.saturating_sub(1))
}

fn word_back(editor: &HexEditor, from: u64, big: bool) -> u64 {
    if matches!(editor.active_pane, Pane::Hex) {
        return from.saturating_sub(1);
    }
    if from == 0 {
        return 0;
    }
    let mut i = from - 1;
    // Skip whitespace before the previous word.
    while i > 0 && byte_at(editor, i).is_some_and(|b| b.is_ascii_whitespace()) {
        i -= 1;
    }
    let target_class = byte_at(editor, i).map(|b| classify(b, big));
    // Walk back while still in the same class.
    while i > 0 {
        let prev = i - 1;
        let prev_class = byte_at(editor, prev).map(|b| classify(b, big));
        if prev_class != target_class {
            break;
        }
        i = prev;
    }
    i
}

fn word_end_forward(editor: &HexEditor, from: u64, source_len: u64, big: bool) -> u64 {
    if matches!(editor.active_pane, Pane::Hex) {
        return (from + 1).min(source_len.saturating_sub(1));
    }
    if source_len == 0 {
        return 0;
    }
    let mut i = from + 1;
    // Skip whitespace forward.
    while i < source_len && byte_at(editor, i).is_some_and(|b| b.is_ascii_whitespace()) {
        i += 1;
    }
    if i >= source_len {
        return source_len.saturating_sub(1);
    }
    let target_class = byte_at(editor, i).map(|b| classify(b, big));
    while i + 1 < source_len {
        let next = i + 1;
        let next_class = byte_at(editor, next).map(|b| classify(b, big));
        if next_class != target_class {
            break;
        }
        i = next;
    }
    i
}

/// Walk back from `cur` to the byte just after the previous newline
/// (`0x0A`), or to 0 if no newline precedes. Used by `0` in the
/// ASCII pane.
fn find_line_start(editor: &HexEditor, cur: u64) -> u64 {
    let mut i = cur;
    while i > 0 {
        let prev = i - 1;
        if byte_at(editor, prev) == Some(b'\n') {
            return i;
        }
        i = prev;
    }
    0
}

/// Walk forward from `cur` to the byte just before the next newline
/// (`0x0A`), or to the last byte if no newline follows. Used by `$`
/// in the ASCII pane.
fn find_line_end(editor: &HexEditor, cur: u64, source_len: u64) -> u64 {
    if source_len == 0 {
        return 0;
    }
    let mut i = cur;
    while i + 1 < source_len {
        let next = i + 1;
        if byte_at(editor, next) == Some(b'\n') {
            return i;
        }
        i = next;
    }
    source_len - 1
}

/// Make sure a selection exists with anchor == cursor. Used when
/// entering Visual without a prior selection so the next motion has
/// something to extend from.
fn ensure_anchor(editor: &mut HexEditor) {
    if editor.selection.is_none() {
        editor.selection = Some(Selection::caret(ByteOffset::new(0)));
    }
    if let Some(sel) = editor.selection.as_mut() {
        sel.anchor = sel.cursor;
    }
}

/// Snap the selection to whole rows (hex) or whole lines (ASCII)
/// around the current cursor. Called when entering VisualLine so the
/// initial highlight visibly matches the linewise mode.
fn snap_visual_line(editor: &mut HexEditor, columns: u64, source_len: u64) {
    let cur = current_cursor(editor).unwrap_or(0);
    let (start, end_inclusive) = line_bounds(editor, cur, columns, source_len);
    if let Some(sel) = editor.selection.as_mut() {
        sel.anchor = ByteOffset::new(start);
        sel.cursor = ByteOffset::new(end_inclusive);
    }
}

/// Inclusive byte bounds for the row/line containing `cur`. Hex pane
/// uses fixed `columns`-wide rows; ASCII pane uses LF boundaries.
fn line_bounds(editor: &HexEditor, cur: u64, columns: u64, source_len: u64) -> (u64, u64) {
    if source_len == 0 {
        return (0, 0);
    }
    match editor.active_pane {
        Pane::Hex => {
            let start = (cur / columns) * columns;
            let end = (start + columns - 1).min(source_len - 1);
            (start, end)
        }
        Pane::Ascii => {
            let start = find_line_start(editor, cur);
            let end = find_line_end(editor, cur, source_len);
            (start, end)
        }
    }
}

/// Read `[start, end_exclusive)` from the patched source. Returns an
/// empty Vec on bounds mismatch -- callers treat that as "nothing to
/// yank/delete" rather than propagating an error to the user.
fn read_range_bytes(editor: &HexEditor, start: u64, end_exclusive: u64) -> Vec<u8> {
    if end_exclusive <= start {
        return Vec::new();
    }
    let Ok(range) = hxy_core::ByteRange::new(ByteOffset::new(start), ByteOffset::new(end_exclusive))
    else {
        return Vec::new();
    };
    editor.source.read(range).unwrap_or_default()
}

/// Format yanked bytes for the system clipboard. Hex pane yanks land
/// as space-separated uppercase hex (matching the existing copy-hex
/// menu); ASCII pane yanks land as utf-8 lossy text.
fn format_for_clipboard(bytes: &[u8], origin: RegisterOrigin) -> String {
    match origin {
        RegisterOrigin::Hex => bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" "),
        RegisterOrigin::Ascii => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn current_register_origin(editor: &HexEditor) -> RegisterOrigin {
    match editor.active_pane {
        Pane::Hex => RegisterOrigin::Hex,
        Pane::Ascii => RegisterOrigin::Ascii,
    }
}

/// Yank the live Visual / VisualLine selection. Caller is responsible
/// for switching back to Normal afterwards.
fn yank_selection(editor: &mut HexEditor, ctx: &egui::Context) {
    let Some(sel) = editor.selection else {
        return;
    };
    let range = sel.range();
    let bytes = read_range_bytes(editor, range.start().get(), range.end().get());
    if bytes.is_empty() {
        return;
    }
    stash_register(editor, ctx, bytes);
    if let Some(sel) = editor.selection.as_mut() {
        sel.anchor = sel.cursor;
    }
}

/// `yy` / `<count>yy`: yank `count` rows / lines starting at the row
/// or line containing the cursor.
fn yank_rows(editor: &mut HexEditor, ctx: &egui::Context, count: usize, columns: u64, source_len: u64) {
    let cur = current_cursor(editor).unwrap_or(0);
    let (start, end_inclusive) = multi_line_bounds(editor, cur, count, columns, source_len);
    let bytes = read_range_bytes(editor, start, end_inclusive + 1);
    if bytes.is_empty() {
        return;
    }
    stash_register(editor, ctx, bytes);
}

/// Save bytes to the unnamed register and write the formatted form to
/// the system clipboard.
fn stash_register(editor: &mut HexEditor, ctx: &egui::Context, bytes: Vec<u8>) {
    let origin = current_register_origin(editor);
    let text = format_for_clipboard(&bytes, origin);
    ctx.copy_text(text);
    editor.vim.register = bytes;
    editor.vim.register_origin = origin;
}

/// Inclusive bounds spanning `count` consecutive rows / lines starting
/// at the one containing `cur`. Used by `yy` / `dd` with a count.
fn multi_line_bounds(editor: &HexEditor, cur: u64, count: usize, columns: u64, source_len: u64) -> (u64, u64) {
    let (start, mut end) = line_bounds(editor, cur, columns, source_len);
    for _ in 1..count {
        if end + 1 >= source_len {
            break;
        }
        let (_, next_end) = line_bounds(editor, end + 1, columns, source_len);
        end = next_end;
    }
    (start, end)
}

/// Delete the live Visual / VisualLine selection: stash it in the
/// register and splice it out of the underlying source.
fn delete_selection(editor: &mut HexEditor, ctx: &egui::Context) {
    let Some(sel) = editor.selection else {
        return;
    };
    let range = sel.range();
    let start = range.start().get();
    let end = range.end().get();
    let bytes = read_range_bytes(editor, start, end);
    if bytes.is_empty() {
        return;
    }
    stash_register(editor, ctx, bytes);
    #[cfg(feature = "editor")]
    {
        let len = end - start;
        let _ = editor.splice(start, len, Vec::new());
    }
    let new_len = editor.source.len().get();
    let new_cursor = if new_len == 0 { 0 } else { start.min(new_len - 1) };
    editor.selection = Some(Selection::caret(ByteOffset::new(new_cursor)));
}

/// `dd` / `<count>dd`: delete `count` rows / lines.
fn delete_rows(editor: &mut HexEditor, ctx: &egui::Context, count: usize, columns: u64, source_len: u64) {
    let cur = current_cursor(editor).unwrap_or(0);
    let (start, end_inclusive) = multi_line_bounds(editor, cur, count, columns, source_len);
    let bytes = read_range_bytes(editor, start, end_inclusive + 1);
    if bytes.is_empty() {
        return;
    }
    stash_register(editor, ctx, bytes);
    #[cfg(feature = "editor")]
    {
        let len = end_inclusive + 1 - start;
        let _ = editor.splice(start, len, Vec::new());
    }
    let new_len = editor.source.len().get();
    let new_cursor = if new_len == 0 { 0 } else { start.min(new_len - 1) };
    editor.selection = Some(Selection::caret(ByteOffset::new(new_cursor)));
}

/// `x` / `<count>x`: delete `count` bytes starting at the cursor.
fn delete_byte_under_cursor(editor: &mut HexEditor, ctx: &egui::Context, count: usize) {
    let cur = current_cursor(editor).unwrap_or(0);
    let source_len = editor.source.len().get();
    if source_len == 0 || cur >= source_len {
        return;
    }
    let len = (count as u64).min(source_len - cur);
    let bytes = read_range_bytes(editor, cur, cur + len);
    if bytes.is_empty() {
        return;
    }
    stash_register(editor, ctx, bytes);
    #[cfg(feature = "editor")]
    {
        let _ = editor.splice(cur, len, Vec::new());
    }
    let new_len = editor.source.len().get();
    let new_cursor = if new_len == 0 { 0 } else { cur.min(new_len - 1) };
    editor.selection = Some(Selection::caret(ByteOffset::new(new_cursor)));
}

/// `p`: paste the unnamed register `count` times after the cursor
/// (vim's paste-after default). On an empty buffer paste lands at 0.
fn paste_register(editor: &mut HexEditor, count: usize) {
    if editor.vim.register.is_empty() {
        return;
    }
    let cur = current_cursor(editor).unwrap_or(0);
    let source_len = editor.source.len().get();
    let insert_at = if source_len == 0 { 0 } else { (cur + 1).min(source_len) };
    let mut payload = Vec::with_capacity(editor.vim.register.len() * count);
    for _ in 0..count {
        payload.extend_from_slice(&editor.vim.register);
    }
    let payload_len = payload.len() as u64;
    #[cfg(feature = "editor")]
    {
        let _ = editor.splice(insert_at, 0, payload);
    }
    #[cfg(not(feature = "editor"))]
    let _ = payload;
    let new_len = editor.source.len().get();
    if new_len == 0 {
        editor.selection = Some(Selection::caret(ByteOffset::new(0)));
        return;
    }
    let landing = insert_at + payload_len.saturating_sub(1);
    let landing = landing.min(new_len - 1);
    editor.selection = Some(Selection::caret(ByteOffset::new(landing)));
}
