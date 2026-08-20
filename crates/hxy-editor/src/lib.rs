//! Framework-agnostic hex-editor model. UI layers (egui: hxy-view,
//! gpui: hxy-view-gpui) translate native events into [`InputEvent`]s
//! and render from [`HexEditor`] state.

#![forbid(unsafe_code)]

mod events;

#[cfg(feature = "editor")]
mod editor;
mod dispatch;
mod input;
mod vim;

pub use events::Disposition;
pub use events::Effect;
pub use events::InputEvent;
pub use events::Key;
pub use events::Modifiers;

pub use dispatch::InputBatch;
pub use dispatch::InputFilter;
#[cfg(feature = "editor")]
pub use editor::EditEntry;
#[cfg(feature = "editor")]
pub use editor::EditMode;
#[cfg(feature = "editor")]
pub use editor::TypingMode;
#[cfg(feature = "editor")]
pub use editor::WriteError;
pub use input::SCROLLOFF_ROWS;
pub use vim::FindDir;
pub use vim::InputMode;
pub use vim::Pending;
pub use vim::RegisterOrigin;
pub use vim::VimMode;
pub use vim::VimState;

use std::sync::Arc;

use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::ColumnCount;
use hxy_core::HexSource;
use hxy_core::Selection;

/// Which of the two row panes a pointer interacted with, or which
/// one the caller's editor currently treats as "active" for styling
/// purposes. Consumers can use this to route keystrokes (hex digits
/// vs ASCII characters) and to visually mark the inactive pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Hex,
    Ascii,
}

/// Which half of the byte under the cursor the next typed hex digit
/// lands on. `None` from [`HexEditor::view_parts`] means no nibble
/// cursor should render (readonly editor or editor feature off).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NibbleCursor {
    High,
    Low,
}

/// Borrowed view state a UI adapter needs to render one frame.
/// Produced by [`HexEditor::view_parts`]. `selection` is mutable so
/// the renderer can update the caret from click / drag interaction;
/// `pending_scroll` / `pending_scroll_to_byte` are taken (cleared) on
/// each call so a scroll request fires exactly once.
pub struct ViewParts<'e> {
    pub source: &'e std::sync::Arc<dyn HexSource>,
    pub selection: &'e mut Option<Selection>,
    pub active_pane: Pane,
    /// [`NibbleCursor::High`] when the next hex-digit keystroke
    /// overwrites the high nibble, [`NibbleCursor::Low`] for the low
    /// nibble. `None` when the editor feature is off or the editor is
    /// read-only.
    pub nibble: Option<NibbleCursor>,
    pub pending_scroll: Option<f32>,
    pub pending_scroll_to_byte: Option<ByteOffset>,
}

/// Persistent hex-editor model. Consumers keep one on their tab /
/// document struct between frames and drive it through
/// [`Self::input_filter`] / [`Self::apply_input`] once per frame,
/// then render from [`Self::view_parts`].
///
/// Owns the byte source, selection, scroll state, and -- with the
/// `editor` feature enabled -- a writable patch overlay plus
/// undo/redo history. Strip the editor bits by building with
/// `default-features = false` if you only need a read-only viewer.
pub struct HexEditor {
    /// Source exposed to renders. When the `editor` feature is on
    /// this is the patched view (base + patch overlay) from
    /// [`editor::EditState`]; otherwise it is the base source the
    /// caller supplied.
    source: Arc<dyn HexSource>,
    pub(crate) selection: Option<Selection>,
    active_pane: Pane,
    /// Cursor offset observed at the end of the previous frame.
    /// Compared each frame to detect cursor moves originating
    /// outside the input dispatcher (mouse click, programmatic
    /// jumps) so the nibble pointer can reset cleanly.
    pub(crate) last_cursor_offset: Option<u64>,
    /// Scroll offset (content pixels) at the end of the previous
    /// frame. Exposed via [`Self::scroll_offset`] so consumers can
    /// persist it across sessions.
    scroll_offset: f32,
    pending_scroll: Option<f32>,
    pending_scroll_to_byte: Option<ByteOffset>,
    /// Columns rendered by the most recent frame. The input
    /// dispatcher uses this for Up/Down navigation; if no frame has
    /// run yet we fall back to [`ColumnCount::DEFAULT`].
    pub(crate) last_columns: Option<ColumnCount>,
    /// Byte range actually rendered last frame (after clipping).
    /// Lets callers check whether a target offset is already on
    /// screen before issuing a `scroll_to_byte` -- e.g. the command
    /// palette's Go-To uses it to skip a disorienting snap when the
    /// user jumps to a nearby offset that's already visible.
    last_visible_range: Option<ByteRange>,
    #[cfg(feature = "editor")]
    pub(crate) edit: editor::EditState,
    /// Top-level input style. `Default` runs the standard
    /// arrow-key dispatcher; `Vim` routes through the modal
    /// state machine in [`vim`].
    input_mode: InputMode,
    /// Vim-mode state. Persists across mode switches so flipping
    /// from `Default` to `Vim` mid-session resumes wherever the
    /// user left off; flipping back doesn't drop the buffer.
    pub(crate) vim: VimState,
}

impl HexEditor {
    /// Build a fresh editor wrapping `source`. With the `editor`
    /// feature on, reads flow through a newly-allocated
    /// [`suture::Patch`] overlay so subsequent writes accumulate
    /// there without mutating the caller's source.
    pub fn new(source: Arc<dyn HexSource>) -> Self {
        #[cfg(feature = "editor")]
        {
            let edit = editor::EditState::new(source);
            Self {
                source: edit.patched_source.clone(),
                selection: None,
                active_pane: Pane::Hex,
                last_cursor_offset: None,
                scroll_offset: 0.0,
                pending_scroll: None,
                pending_scroll_to_byte: None,
                last_columns: None,
                last_visible_range: None,
                edit,
                input_mode: InputMode::Default,
                vim: VimState::default(),
            }
        }
        #[cfg(not(feature = "editor"))]
        {
            Self {
                source,
                selection: None,
                active_pane: Pane::Hex,
                last_cursor_offset: None,
                scroll_offset: 0.0,
                pending_scroll: None,
                pending_scroll_to_byte: None,
                last_columns: None,
                last_visible_range: None,
                input_mode: InputMode::Default,
                vim: VimState::default(),
            }
        }
    }

    /// Current top-level input style. See [`InputMode`].
    pub fn input_mode(&self) -> InputMode {
        self.input_mode
    }

    /// Switch input style. Flipping to `Vim` resets the mode to
    /// `VimMode::Normal` so the user starts in a well-defined
    /// state; flipping back to `Default` leaves any pending count
    /// buffer / sub-mode alone (it's harmless once the dispatcher
    /// stops looking at it).
    pub fn set_input_mode(&mut self, mode: InputMode) {
        if self.input_mode == mode {
            return;
        }
        self.input_mode = mode;
        if matches!(mode, InputMode::Vim) {
            self.vim.mode = VimMode::Normal;
            self.vim.clear_pending();
        }
        // Reset typing back to Replace whenever the input mode is
        // toggled. Vim Insert mode flips this to Insert on entry and
        // back to Replace on Esc, but a mid-Insert toggle of the
        // input mode itself shouldn't leave the editor stuck in the
        // splice-on-every-keystroke path.
        #[cfg(feature = "editor")]
        self.set_typing_mode(crate::editor::TypingMode::Replace);
    }

    /// Read-only access to the Vim state (current sub-mode, count
    /// buffer, ...). Library consumers can use this to render their
    /// own status indicator.
    pub fn vim_state(&self) -> &VimState {
        &self.vim
    }

    /// The user-visible source. With the `editor` feature this is
    /// the patched view; without, it is the caller's original
    /// source.
    pub fn source(&self) -> &Arc<dyn HexSource> {
        &self.source
    }

    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    pub fn set_selection(&mut self, selection: Option<Selection>) {
        self.selection = selection;
        self.last_cursor_offset = selection.map(|s| s.cursor.get());
        #[cfg(feature = "editor")]
        {
            self.edit.edit_high_nibble = true;
            self.edit.history_break = true;
        }
    }

    pub fn active_pane(&self) -> Pane {
        self.active_pane
    }

    /// Which nibble the next hex-digit keystroke would overwrite, or
    /// `None` when the editor feature is off or the editor is
    /// read-only. Read-only mirror of the value [`Self::view_parts`]
    /// reports, so renderers that only have `&self` (e.g. a paint pass)
    /// can draw the nibble caret without taking `&mut`.
    #[cfg(feature = "editor")]
    pub fn nibble(&self) -> Option<NibbleCursor> {
        (self.edit.mode == EditMode::Mutable)
            .then_some(if self.edit.edit_high_nibble { NibbleCursor::High } else { NibbleCursor::Low })
    }

    #[cfg(not(feature = "editor"))]
    pub fn nibble(&self) -> Option<NibbleCursor> {
        None
    }

    pub fn set_active_pane(&mut self, pane: Pane) {
        if self.active_pane != pane {
            self.active_pane = pane;
            #[cfg(feature = "editor")]
            {
                self.edit.edit_high_nibble = true;
                self.edit.history_break = true;
            }
        }
    }

    pub fn scroll_offset(&self) -> f32 {
        self.scroll_offset
    }

    pub fn set_scroll_to(&mut self, offset: f32) {
        self.pending_scroll = Some(offset);
    }

    pub fn set_scroll_to_byte(&mut self, byte: ByteOffset) {
        self.pending_scroll_to_byte = Some(byte);
    }

    /// Schedule a scroll that places the current cursor inside the
    /// viewport with a `scrolloff`-row buffer at the top and bottom
    /// edges (vim's `'scrolloff'` semantics). No-op when the cursor is
    /// already inside the safe zone, or when no frame has rendered
    /// yet (we need the previous frame's visible range to compute the
    /// target row). When the viewport is too short for the requested
    /// buffer, `scrolloff` is clamped to half the viewport height.
    pub fn ensure_cursor_visible_with_scrolloff(&mut self, scrolloff: u64) {
        let Some(cursor) = self.selection.as_ref().map(|s| s.cursor.get()) else { return };
        let Some(visible) = self.last_visible_range else { return };
        let Some(columns) = self.last_columns else { return };
        let cols = u64::from(columns.get());
        if cols == 0 {
            return;
        }
        let cursor_row = cursor / cols;
        let visible_start_row = visible.start().get() / cols;
        // `visible.end()` is the exclusive byte end of the last
        // partially-visible row. Round up to row-exclusive.
        let visible_end_row = visible.end().get().div_ceil(cols);
        let visible_rows = visible_end_row.saturating_sub(visible_start_row);
        if visible_rows == 0 {
            return;
        }
        let scrolloff = scrolloff.min(visible_rows / 2);
        let safe_start = visible_start_row + scrolloff;
        let safe_end = visible_end_row.saturating_sub(scrolloff);
        if cursor_row >= safe_start && cursor_row < safe_end {
            return;
        }
        let target_top_row = if cursor_row < safe_start {
            cursor_row.saturating_sub(scrolloff)
        } else {
            // cursor at or past the bottom safe edge: place it
            // `scrolloff` rows above the viewport's last row.
            cursor_row.saturating_add(1).saturating_add(scrolloff).saturating_sub(visible_rows)
        };
        self.set_scroll_to_byte(ByteOffset::new(target_top_row.saturating_mul(cols)));
    }

    #[cfg(feature = "editor")]
    pub fn base_source(&self) -> &Arc<dyn HexSource> {
        &self.edit.base_source
    }

    /// Swap in a fresh base source and clear all history. Used
    /// after a successful save so reads reflect the just-written
    /// bytes instead of the stale pre-save buffer.
    #[cfg(feature = "editor")]
    pub fn swap_source(&mut self, base: Arc<dyn HexSource>) {
        self.edit.swap_base(base);
        self.source = self.edit.patched_source.clone();
    }

    /// Swap in a fresh base source while preserving the current
    /// patch. Undo / redo history is dropped because its
    /// `old_bytes` entries reference the previous base and would
    /// be inconsistent against the new one. Used by the
    /// "reload from disk, keep my edits" flow after the file
    /// changes externally -- the user's splices stay applied on
    /// top of the new disk content.
    #[cfg(feature = "editor")]
    pub fn swap_source_keep_patch(&mut self, base: Arc<dyn HexSource>) {
        let saved_patch = self.edit.patch.read().expect("patch lock poisoned").clone();
        self.edit.swap_base(base);
        *self.edit.patch.write().expect("patch lock poisoned") = saved_patch;
        self.source = self.edit.patched_source.clone();
    }

    /// Shared handle into the editor's patch. Callers can clone
    /// this to persist unsaved edits in their own storage layer.
    #[cfg(feature = "editor")]
    pub fn patch(&self) -> &Arc<std::sync::RwLock<suture::Patch>> {
        &self.edit.patch
    }

    #[cfg(feature = "editor")]
    pub fn edit_mode(&self) -> EditMode {
        self.edit.mode
    }

    #[cfg(feature = "editor")]
    pub fn set_edit_mode(&mut self, mode: EditMode) {
        self.edit.mode = mode;
        self.edit.edit_high_nibble = true;
        self.edit.history_break = true;
    }

    #[cfg(feature = "editor")]
    pub fn is_readonly(&self) -> bool {
        matches!(self.edit.mode, EditMode::Readonly)
    }

    #[cfg(feature = "editor")]
    pub fn is_dirty(&self) -> bool {
        self.edit.is_dirty()
    }

    #[cfg(feature = "editor")]
    pub fn modified_ranges(&self) -> Vec<(u64, u64)> {
        self.edit.modified_ranges()
    }

    /// Monotonic count of content mutations (writes, splices, undo,
    /// redo, revert), starting at 0. Unlike [`Self::modified_ranges`]
    /// it changes even when a mutation leaves the range set equal
    /// (e.g. overwriting an already-patched byte with a new value),
    /// so consumers can use it as a cheap "bytes changed" signal.
    #[cfg(feature = "editor")]
    pub fn revision(&self) -> u64 {
        self.edit.revision
    }

    #[cfg(feature = "editor")]
    pub fn undo_stack(&self) -> &[EditEntry] {
        &self.edit.undo_stack
    }

    #[cfg(feature = "editor")]
    pub fn redo_stack(&self) -> &[EditEntry] {
        &self.edit.redo_stack
    }

    /// Replace the editor's undo stack wholesale. Used by
    /// persistence layers that restore a saved session's history.
    #[cfg(feature = "editor")]
    pub fn set_undo_stack(&mut self, stack: Vec<EditEntry>) {
        self.edit.undo_stack = stack;
        self.edit.history_break = true;
    }

    #[cfg(feature = "editor")]
    pub fn set_redo_stack(&mut self, stack: Vec<EditEntry>) {
        self.edit.redo_stack = stack;
    }

    #[cfg(feature = "editor")]
    pub fn can_undo(&self) -> bool {
        !self.edit.undo_stack.is_empty()
    }

    #[cfg(feature = "editor")]
    pub fn can_redo(&self) -> bool {
        !self.edit.redo_stack.is_empty()
    }

    /// Reset the two-press nibble cursor to "expecting high
    /// nibble". Called automatically on navigation; exposed so
    /// consumers can cancel a half-typed byte from a menu action.
    #[cfg(feature = "editor")]
    pub fn reset_edit_nibble(&mut self) {
        self.edit.edit_high_nibble = true;
    }

    /// Force the next write to start a fresh undo entry rather
    /// than coalesce into the previous one. Handy for menu actions
    /// that shouldn't merge with typing (e.g. paste).
    #[cfg(feature = "editor")]
    pub fn push_history_boundary(&mut self) {
        self.edit.history_break = true;
    }

    /// Replace bytes `[offset, offset + remove)` with `insert`.
    /// Generalises in-place writes (`remove == insert.len()`),
    /// inserts (`remove == 0`), and deletes (`insert.is_empty()`).
    /// Used by Vim mode's paste / delete operators; also useful to
    /// library consumers building their own bulk-edit commands.
    #[cfg(feature = "editor")]
    pub fn splice(&mut self, offset: u64, remove: u64, insert: Vec<u8>) -> Result<(), WriteError> {
        self.edit.splice(offset, remove, insert)
    }

    /// Apply a batch of non-overlapping splices as a single undo
    /// entry. Used by find/replace's Replace All so the whole batch
    /// undoes in one keystroke. `ops` must be sorted by offset and
    /// non-overlapping; each op is
    /// `(offset, remove_len, insert_bytes)`. Returns `Err` if any op
    /// is out of bounds or if the batch fails the ordering check;
    /// in that case the source is untouched.
    #[cfg(feature = "editor")]
    pub fn splice_many(&mut self, ops: &[(u64, u64, Vec<u8>)]) -> Result<(), WriteError> {
        self.pin_scroll_for_next_frame();
        self.edit.splice_many(ops)
    }

    #[cfg(not(feature = "editor"))]
    pub(crate) fn push_history_boundary(&mut self) {}

    /// Record a length-preserving write at `offset`. Gated by the
    /// editor's [`EditMode`]; writes past EOF are rejected.
    #[cfg(feature = "editor")]
    pub fn request_write(&mut self, offset: u64, bytes: Vec<u8>) -> Result<(), WriteError> {
        self.pin_scroll_for_next_frame();
        self.edit.request_write(offset, bytes)
    }

    /// Re-pend the current scroll offset as a pending target for the
    /// next render. The scroll area's memory has a habit of losing
    /// its position around events that rebuild the hex pane's
    /// geometry (fresh `byte_styler`, patch mutations, pane-focus
    /// changes); without this pin, the view would snap back to the
    /// top after each edit.
    #[cfg(feature = "editor")]
    fn pin_scroll_for_next_frame(&mut self) {
        if self.pending_scroll.is_none() && self.scroll_offset > 0.0 {
            self.pending_scroll = Some(self.scroll_offset);
        }
    }

    /// Apply one hex-digit keystroke at the current cursor offset.
    /// Two presses compose one byte: the first overwrites the
    /// high nibble, the second the low. Returns `true` when a
    /// full byte has been completed so callers can advance the
    /// cursor.
    #[cfg(feature = "editor")]
    pub fn type_hex_digit(&mut self, nibble: u8) -> Result<bool, WriteError> {
        if self.edit.mode != EditMode::Mutable {
            return Err(WriteError::Readonly);
        }
        let nibble = nibble & 0xF;
        let Some(sel) = self.selection else { return Ok(false) };
        let offset = sel.cursor.get();
        let source_len = self.source.len().get();
        // First-nibble press creates a new byte when typing-mode is
        // Insert *or* the cursor is past EOF. The follow-up
        // low-nibble press fills its low half via the in-place
        // rewrite branch below (the just-created byte is now
        // in-bounds).
        let creating_new_byte =
            self.edit.edit_high_nibble && (self.edit.typing_mode == TypingMode::Insert || offset >= source_len);
        if creating_new_byte {
            self.pin_scroll_for_next_frame();
            self.edit.insert_at(offset, vec![nibble << 4])?;
            self.edit.edit_high_nibble = false;
            return Ok(false);
        }
        if offset >= source_len {
            // Defensive: a low-nibble press past EOF in Replace mode
            // shouldn't reach here since `edit_high_nibble` resets to
            // true on cursor moves, but bail safely if it does.
            return Ok(false);
        }
        let current = self.edit.read_byte_at(offset)?;
        let new_byte =
            if self.edit.edit_high_nibble { (nibble << 4) | (current & 0x0F) } else { (current & 0xF0) | nibble };
        self.pin_scroll_for_next_frame();
        self.edit.request_write(offset, vec![new_byte])?;
        let advanced = !self.edit.edit_high_nibble;
        self.edit.edit_high_nibble = !self.edit.edit_high_nibble;
        Ok(advanced)
    }

    /// Write one ASCII byte at the cursor. Returns `true` when a
    /// write was issued so the caller can advance the cursor.
    #[cfg(feature = "editor")]
    pub fn type_ascii_byte(&mut self, byte: u8) -> Result<bool, WriteError> {
        if self.edit.mode != EditMode::Mutable {
            return Err(WriteError::Readonly);
        }
        let Some(sel) = self.selection else { return Ok(false) };
        let offset = sel.cursor.get();
        let source_len = self.source.len().get();
        self.pin_scroll_for_next_frame();
        let insert = offset >= source_len || self.edit.typing_mode == TypingMode::Insert;
        if insert {
            self.edit.insert_at(offset, vec![byte])?;
        } else {
            self.edit.request_write(offset, vec![byte])?;
        }
        Ok(true)
    }

    /// Insert-mode Backspace: splice out the byte before the cursor
    /// and step back. No-op at offset 0. Returns `true` if a byte
    /// was removed (callers may want to refresh layout / scroll).
    #[cfg(feature = "editor")]
    pub fn backspace_byte(&mut self) -> Result<bool, WriteError> {
        if self.edit.mode != EditMode::Mutable {
            return Err(WriteError::Readonly);
        }
        let Some(sel) = self.selection else { return Ok(false) };
        let offset = sel.cursor.get();
        if offset == 0 {
            return Ok(false);
        }
        self.pin_scroll_for_next_frame();
        self.edit.splice(offset - 1, 1, Vec::new())?;
        if let Some(sel) = self.selection.as_mut() {
            sel.cursor = ByteOffset::new(offset - 1);
            sel.anchor = sel.cursor;
        }
        self.edit.edit_high_nibble = true;
        Ok(true)
    }

    /// Current typing mode -- whether keystrokes overwrite in place
    /// or splice in new bytes.
    #[cfg(feature = "editor")]
    pub fn typing_mode(&self) -> TypingMode {
        self.edit.typing_mode
    }

    /// Switch typing mode. Resets the half-typed-nibble state so a
    /// mode flip mid-byte doesn't leave a stale low-nibble pointer.
    #[cfg(feature = "editor")]
    pub fn set_typing_mode(&mut self, mode: TypingMode) {
        self.edit.typing_mode = mode;
        self.edit.edit_high_nibble = true;
        self.edit.history_break = true;
    }

    /// Pop the most recent undo entry, revert the patch to match
    /// the remaining stack, and push the popped entry onto redo.
    /// Returns the reverted entry so callers can realign UI state
    /// (e.g. scroll the cursor back to the change site).
    #[cfg(feature = "editor")]
    pub fn undo(&mut self) -> Option<EditEntry> {
        self.edit.undo()
    }

    #[cfg(feature = "editor")]
    pub fn redo(&mut self) -> Option<EditEntry> {
        self.edit.redo()
    }

    /// Drop all pending edits and both history stacks.
    #[cfg(feature = "editor")]
    pub fn revert(&mut self) {
        self.edit.revert();
    }

    /// Build an [`InputFilter`] seeded from the editor's current
    /// input mode and pane / edit snapshots. Adapters feed native
    /// events into the filter, then hand the resulting
    /// [`InputBatch`] to [`Self::apply_input`].
    pub fn input_filter(&self) -> InputFilter {
        #[cfg(feature = "editor")]
        let mutable = self.edit.mode == editor::EditMode::Mutable;
        #[cfg(not(feature = "editor"))]
        let mutable = false;
        #[cfg(feature = "editor")]
        let inserting = self.edit.typing_mode == editor::TypingMode::Insert;
        #[cfg(not(feature = "editor"))]
        let inserting = false;
        InputFilter::new(self.input_mode, self.vim.mode, self.vim.pending, self.active_pane, mutable, inserting)
    }

    /// Apply a batch of filtered input to the editor, returning any
    /// side effects (clipboard writes) the adapter must execute.
    /// Runs its once-per-frame bookkeeping even for an empty batch,
    /// so adapters call [`Self::input_filter`] / `apply_input`
    /// unconditionally every frame.
    pub fn apply_input(&mut self, batch: InputBatch) -> Vec<Effect> {
        dispatch::apply(self, batch)
    }

    /// Borrow the state a UI adapter needs to render one frame.
    /// Clears any pending scroll request so it fires exactly once.
    pub fn view_parts(&mut self) -> ViewParts<'_> {
        let pending_scroll = self.pending_scroll.take();
        let pending_scroll_to_byte = self.pending_scroll_to_byte.take();
        let nibble = self.nibble();
        ViewParts {
            source: &self.source,
            selection: &mut self.selection,
            active_pane: self.active_pane,
            nibble,
            pending_scroll,
            pending_scroll_to_byte,
        }
    }

    /// Latch a just-rendered frame's geometry into persistent editor
    /// state: scroll offset, the column count the next input frame
    /// uses for Up/Down navigation, the visible byte range, and the
    /// pane the user interacted with (if any).
    pub fn on_frame(
        &mut self,
        scroll_offset: f32,
        columns: ColumnCount,
        visible_range: Option<ByteRange>,
        interacted_pane: Option<Pane>,
    ) {
        self.scroll_offset = scroll_offset;
        self.last_columns = Some(columns);
        self.last_visible_range = visible_range;
        if let Some(pane) = interacted_pane {
            self.set_active_pane(pane);
        }
    }

    /// `true` when `offset` lies inside the byte range the last
    /// rendered frame actually painted (after scroll-area clipping).
    /// Returns `false` if no frame has been rendered yet. Callers
    /// use this to skip a disorienting scroll snap when a navigation
    /// target is already on screen.
    pub fn is_offset_visible(&self, offset: ByteOffset) -> bool {
        let Some(range) = self.last_visible_range else { return false };
        offset.get() >= range.start().get() && offset.get() < range.end().get()
    }
}

#[cfg(all(test, feature = "editor"))]
mod typing_tests {
    use super::*;
    use hxy_core::MemorySource;

    fn ed_with(bytes: &[u8], cursor: u64) -> HexEditor {
        let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes.to_vec()));
        let mut ed = HexEditor::new(source);
        ed.set_active_pane(Pane::Ascii);
        ed.selection = Some(Selection::caret(ByteOffset::new(cursor)));
        ed
    }

    fn read_all_bytes(ed: &HexEditor) -> Vec<u8> {
        let len = ed.source.len().get();
        if len == 0 {
            return Vec::new();
        }
        let r = ByteRange::new(ByteOffset::new(0), ByteOffset::new(len)).unwrap();
        ed.source.read(r).unwrap()
    }

    #[test]
    fn replace_typing_overwrites_in_place() {
        let mut ed = ed_with(b"abcde", 1);
        ed.set_typing_mode(TypingMode::Replace);
        ed.type_ascii_byte(b'X').unwrap();
        assert_eq!(read_all_bytes(&ed), b"aXcde");
    }

    #[test]
    fn replace_typing_grows_past_eof() {
        let mut ed = ed_with(b"abc", 3);
        ed.set_typing_mode(TypingMode::Replace);
        ed.type_ascii_byte(b'X').unwrap();
        assert_eq!(read_all_bytes(&ed), b"abcX");
    }

    #[test]
    fn insert_typing_splices_in_bounds() {
        let mut ed = ed_with(b"abcde", 1);
        ed.set_typing_mode(TypingMode::Insert);
        ed.type_ascii_byte(b'X').unwrap();
        assert_eq!(read_all_bytes(&ed), b"aXbcde");
    }

    #[test]
    fn backspace_byte_deletes_before_cursor() {
        let mut ed = ed_with(b"abcde", 3);
        ed.backspace_byte().unwrap();
        assert_eq!(read_all_bytes(&ed), b"abde");
        assert_eq!(ed.selection.unwrap().cursor.get(), 2);
    }

    #[test]
    fn backspace_byte_at_zero_is_noop() {
        let mut ed = ed_with(b"abc", 0);
        let removed = ed.backspace_byte().unwrap();
        assert!(!removed);
        assert_eq!(read_all_bytes(&ed), b"abc");
    }

    #[test]
    fn insert_hex_first_nibble_grows_then_low_nibble_fills() {
        let mut ed = ed_with(&[0xAA, 0xBB, 0xCC], 1);
        ed.set_typing_mode(TypingMode::Insert);
        ed.type_hex_digit(0x3).unwrap();
        assert_eq!(read_all_bytes(&ed), &[0xAA, 0x30, 0xBB, 0xCC]);
        ed.type_hex_digit(0xC).unwrap();
        assert_eq!(read_all_bytes(&ed), &[0xAA, 0x3C, 0xBB, 0xCC]);
    }
}
