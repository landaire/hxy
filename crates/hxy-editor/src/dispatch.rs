//! Framework-neutral input dispatch. [`InputFilter`] turns a stream
//! of [`InputEvent`]s into a batch of internal presses, reporting per
//! event whether the editor consumed it. [`apply`] runs the batch
//! against a [`HexEditor`] and returns any [`Effect`]s the adapter
//! must execute.
//!
//! This is the model-side half of the old egui `input::dispatch` /
//! `vim::dispatch` functions: the per-event decision tables live in
//! [`InputFilter::feed`], the once-per-frame bookkeeping and press
//! application live in [`apply`].

use crate::HexEditor;
use crate::Pane;
use crate::events::Disposition;
use crate::events::Effect;
use crate::events::InputEvent;
use crate::events::Key;
use crate::events::Modifiers;
use crate::input::EditPress;
use crate::input::Extend;
use crate::input::HorizStep;
use crate::input::VertStep;
use crate::vim::FindDir;
use crate::vim::InputMode;
use crate::vim::Motion;
use crate::vim::Pending;
use crate::vim::VimMode;
use crate::vim::VimPress;

/// One press collected by [`InputFilter`], unifying the default
/// dispatcher's [`EditPress`] and the Vim dispatcher's [`VimPress`].
enum Press {
    Edit(EditPress),
    Vim(VimPress),
}

/// Per-frame event filter. Built by [`HexEditor::input_filter`] from
/// the editor's current mode / pane snapshots, fed one
/// [`InputEvent`] at a time, then finished into an [`InputBatch`].
pub struct InputFilter {
    input_mode: InputMode,
    vim_mode: VimMode,
    /// Running pending state, seeded from the editor's `vim.pending`.
    /// Mutated as find-char / text-object sequences resolve within a
    /// single frame, mirroring the old retain closure's `local_pending`.
    local_pending: Option<Pending>,
    active_pane: Pane,
    mutable: bool,
    inserting: bool,
    presses: Vec<Press>,
    /// Set when a bare Escape is seen in Vim Insert / Replace mode:
    /// [`apply`] then only pops the mode and drops any presses.
    exit_insert: bool,
    /// Once the Insert / Replace Escape latch fires, every further
    /// event is passed through untouched.
    escaped: bool,
}

/// Opaque result of one frame's filtering. Hand it to
/// [`HexEditor::apply_input`].
pub struct InputBatch {
    presses: Vec<Press>,
    path: ApplyPath,
}

/// Which apply routine a finished batch routes to.
enum ApplyPath {
    /// Default dispatch (also the Vim Insert / Replace delegate path).
    Default,
    /// Vim Normal / Visual apply.
    VimNormal,
    /// Vim Insert / Replace Escape latch: pop the mode, drop presses.
    VimExitInsert,
}

impl InputFilter {
    pub(crate) fn new(
        input_mode: InputMode,
        vim_mode: VimMode,
        pending: Option<Pending>,
        active_pane: Pane,
        mutable: bool,
        inserting: bool,
    ) -> Self {
        Self {
            input_mode,
            vim_mode,
            local_pending: pending,
            active_pane,
            mutable,
            inserting,
            presses: Vec::new(),
            exit_insert: false,
            escaped: false,
        }
    }

    /// Feed one native input event. Returns whether the editor
    /// consumed it; `Passed` events stay with the host UI.
    pub fn feed(&mut self, event: &InputEvent) -> Disposition {
        match self.input_mode {
            InputMode::Default => self.feed_default(event),
            InputMode::Vim => self.feed_vim(event),
        }
    }

    /// Finish the frame and produce the batch to apply.
    pub fn finish(self) -> InputBatch {
        let path = match self.input_mode {
            InputMode::Default => ApplyPath::Default,
            InputMode::Vim => {
                if matches!(self.vim_mode, VimMode::Insert | VimMode::Replace) {
                    if self.exit_insert { ApplyPath::VimExitInsert } else { ApplyPath::Default }
                } else {
                    ApplyPath::VimNormal
                }
            }
        };
        InputBatch { presses: self.presses, path }
    }

    fn push_edit(&mut self, press: EditPress) {
        self.presses.push(Press::Edit(press));
    }

    fn push_vim(&mut self, press: VimPress) {
        self.presses.push(Press::Vim(press));
    }

    fn feed_vim(&mut self, event: &InputEvent) -> Disposition {
        if matches!(self.vim_mode, VimMode::Insert | VimMode::Replace) {
            // Insert / Replace delegate typing to the default
            // dispatcher; a bare Escape latches a pop back to Normal
            // and swallows the rest of the batch.
            if self.escaped {
                return Disposition::Passed;
            }
            if let InputEvent::Key { key: Key::Escape, modifiers } = event
                && *modifiers == Modifiers::default()
            {
                self.exit_insert = true;
                self.escaped = true;
                return Disposition::Consumed;
            }
            return self.feed_default(event);
        }
        self.feed_vim_normal(event)
    }

    fn feed_default(&mut self, event: &InputEvent) -> Disposition {
        // Without the editor feature these snapshots go unread; touch
        // them so they don't warn.
        let _ = (self.mutable, self.inserting, self.active_pane);
        match event {
            InputEvent::Key { key, modifiers } => {
                if modifiers.command || modifiers.alt {
                    return Disposition::Passed;
                }
                #[cfg(feature = "editor")]
                if self.mutable && self.active_pane == Pane::Hex && let Some(nibble) = key.hex_nibble() {
                    self.push_edit(EditPress::Hex(nibble));
                    return Disposition::Consumed;
                }
                let extend = Extend::from_shift(modifiers.shift);
                match key {
                    Key::ArrowLeft => {
                        self.push_edit(EditPress::NavHoriz(HorizStep::Left, extend));
                        Disposition::Consumed
                    }
                    Key::ArrowRight => {
                        self.push_edit(EditPress::NavHoriz(HorizStep::Right, extend));
                        Disposition::Consumed
                    }
                    Key::ArrowUp => {
                        self.push_edit(EditPress::NavVert(VertStep::Up, extend));
                        Disposition::Consumed
                    }
                    Key::ArrowDown => {
                        self.push_edit(EditPress::NavVert(VertStep::Down, extend));
                        Disposition::Consumed
                    }
                    Key::Escape => {
                        self.push_edit(EditPress::ClearSelection);
                        Disposition::Consumed
                    }
                    #[cfg(feature = "editor")]
                    Key::Backspace if self.mutable && self.inserting => {
                        self.push_edit(EditPress::Backspace);
                        Disposition::Consumed
                    }
                    _ => Disposition::Passed,
                }
            }
            InputEvent::Text(s) => {
                #[cfg(feature = "editor")]
                if self.mutable && self.active_pane == Pane::Ascii {
                    let mut consumed = false;
                    for ch in s.chars() {
                        if ch.is_ascii_graphic() || ch == ' ' {
                            self.push_edit(EditPress::Ascii(ch as u8));
                            consumed = true;
                        }
                    }
                    return if consumed { Disposition::Consumed } else { Disposition::Passed };
                }
                let _ = s;
                Disposition::Passed
            }
        }
    }

    fn feed_vim_normal(&mut self, event: &InputEvent) -> Disposition {
        let want_char = matches!(
            self.local_pending,
            Some(Pending::FindChar { .. } | Pending::TextObjectInner | Pending::TextObjectAround)
        );
        match event {
            InputEvent::Key { key, modifiers } => {
                if want_char {
                    // Waiting on a literal character; the accompanying
                    // Text event carries it. Swallow the Key event so
                    // motions like `b` / `w` / `i` don't fire as if we
                    // weren't pending.
                    if *key == Key::Escape {
                        self.push_vim(VimPress::Escape);
                        self.local_pending = None;
                    }
                    return Disposition::Consumed;
                }
                if modifiers.command || modifiers.alt {
                    return Disposition::Passed;
                }
                let shift = modifiers.shift;
                match key {
                    Key::Escape => {
                        self.push_vim(VimPress::Escape);
                        Disposition::Consumed
                    }
                    Key::Tab => {
                        self.push_vim(VimPress::TogglePane);
                        Disposition::Consumed
                    }
                    Key::Letter('h') => {
                        self.push_vim(VimPress::Motion(Motion::Left));
                        Disposition::Consumed
                    }
                    Key::Letter('j') => {
                        self.push_vim(VimPress::Motion(Motion::Down));
                        Disposition::Consumed
                    }
                    Key::Letter('k') => {
                        self.push_vim(VimPress::Motion(Motion::Up));
                        Disposition::Consumed
                    }
                    Key::Letter('l') => {
                        self.push_vim(VimPress::Motion(Motion::Right));
                        Disposition::Consumed
                    }
                    Key::Letter('w') => {
                        self.push_vim(VimPress::Motion(if shift {
                            Motion::WordEndForwardBig
                        } else {
                            Motion::WordForward
                        }));
                        Disposition::Consumed
                    }
                    Key::Letter('b') => {
                        self.push_vim(VimPress::Motion(if shift { Motion::WordBackBig } else { Motion::WordBack }));
                        Disposition::Consumed
                    }
                    Key::Letter('e') => {
                        self.push_vim(VimPress::Motion(if shift {
                            Motion::WordEndForwardBig
                        } else {
                            Motion::WordEndForward
                        }));
                        Disposition::Consumed
                    }
                    Key::Letter('f') => {
                        let dir = if shift { FindDir::Backward } else { FindDir::Forward };
                        self.push_vim(VimPress::StartFindChar { dir, before: false });
                        self.local_pending = Some(Pending::FindChar { dir, before: false });
                        Disposition::Consumed
                    }
                    Key::Letter('t') => {
                        let dir = if shift { FindDir::Backward } else { FindDir::Forward };
                        self.push_vim(VimPress::StartFindChar { dir, before: true });
                        self.local_pending = Some(Pending::FindChar { dir, before: true });
                        Disposition::Consumed
                    }
                    Key::Letter('i') => {
                        if matches!(self.vim_mode, VimMode::Visual | VimMode::VisualLine) {
                            self.push_vim(VimPress::StartTextObject(false));
                            self.local_pending = Some(Pending::TextObjectInner);
                        } else {
                            self.push_vim(VimPress::EnterInsert);
                        }
                        Disposition::Consumed
                    }
                    Key::Letter('r') if shift => {
                        self.push_vim(VimPress::EnterReplace);
                        Disposition::Consumed
                    }
                    Key::Letter('a') if matches!(self.vim_mode, VimMode::Visual | VimMode::VisualLine) => {
                        self.push_vim(VimPress::StartTextObject(true));
                        self.local_pending = Some(Pending::TextObjectAround);
                        Disposition::Consumed
                    }
                    Key::Letter('v') => {
                        self.push_vim(if shift { VimPress::EnterVisualLine } else { VimPress::EnterVisual });
                        Disposition::Consumed
                    }
                    Key::Letter('y') => {
                        self.push_vim(VimPress::Yank);
                        Disposition::Consumed
                    }
                    Key::Letter('p') => {
                        self.push_vim(VimPress::Paste);
                        Disposition::Consumed
                    }
                    Key::Letter('d') => {
                        self.push_vim(VimPress::Delete);
                        Disposition::Consumed
                    }
                    Key::Letter('x') => {
                        self.push_vim(VimPress::DeleteByte);
                        Disposition::Consumed
                    }
                    Key::Letter('g') => {
                        // Capital G = end-of-file; lowercase g starts a
                        // `gg` sequence (resolved on the next keypress).
                        self.push_vim(if shift { VimPress::Motion(Motion::EndOfFile) } else { VimPress::PendingG });
                        Disposition::Consumed
                    }
                    // `$` (Shift+4) -- end of line / row. Must be before
                    // the bare `Digit(4) -> Digit(4)` arm, otherwise the
                    // unguarded arm wins.
                    Key::Digit(4) if shift => {
                        self.push_vim(VimPress::Motion(Motion::LineEnd));
                        Disposition::Consumed
                    }
                    // Pure `0` with no count buffer is the line-start
                    // motion; with a count buffer it's a digit. The apply
                    // step inspects the count buffer and routes accordingly.
                    Key::Digit(d) if !shift && *d <= 9 => {
                        self.push_vim(VimPress::Digit(*d));
                        Disposition::Consumed
                    }
                    _ => Disposition::Passed,
                }
            }
            InputEvent::Text(text) => {
                if want_char && let Some(c) = text.chars().next() {
                    match self.local_pending {
                        Some(Pending::FindChar { dir, before }) => {
                            self.push_vim(VimPress::FindCharResolved { c, dir, before });
                        }
                        Some(Pending::TextObjectInner) => {
                            self.push_vim(VimPress::TextObjectChar { c, around: false });
                        }
                        Some(Pending::TextObjectAround) => {
                            self.push_vim(VimPress::TextObjectChar { c, around: true });
                        }
                        _ => {}
                    }
                    self.local_pending = None;
                }
                // Text events otherwise leak literal letters into any
                // focused text field -- always swallow in Normal / Visual.
                Disposition::Consumed
            }
        }
    }
}

/// Apply a finished batch to `editor`, returning any effects.
pub(crate) fn apply(editor: &mut HexEditor, batch: InputBatch) -> Vec<Effect> {
    match batch.path {
        ApplyPath::Default => {
            let presses = batch
                .presses
                .into_iter()
                .filter_map(|p| match p {
                    Press::Edit(e) => Some(e),
                    Press::Vim(_) => None,
                })
                .collect();
            crate::input::apply(editor, presses);
            Vec::new()
        }
        ApplyPath::VimNormal => {
            let presses = batch
                .presses
                .into_iter()
                .filter_map(|p| match p {
                    Press::Vim(v) => Some(v),
                    Press::Edit(_) => None,
                })
                .collect();
            crate::vim::apply(editor, presses)
        }
        ApplyPath::VimExitInsert => {
            editor.vim.mode = VimMode::Normal;
            #[cfg(feature = "editor")]
            {
                editor.set_typing_mode(crate::editor::TypingMode::Replace);
                editor.reset_edit_nibble();
            }
            Vec::new()
        }
    }
}
