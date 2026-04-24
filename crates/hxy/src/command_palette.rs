//! Thin hxy-specific wrapper around [`egui_palette`].
//!
//! The generic crate handles rendering, fuzzy-matching, and keyboard
//! nav. This module defines hxy's entry / action vocabulary and the
//! cascade mode (Main -> Templates -> ...).

#![cfg(not(target_arch = "wasm32"))]

use std::path::PathBuf;

use crate::file::FileId;

/// Persistent state for the palette. Keeps both the hxy-specific
/// cascade [`Mode`] and the generic [`egui_palette::State`] that
/// owns query / selection / focus.
pub struct PaletteState {
    pub mode: Mode,
    pub inner: egui_palette::State,
}

impl Default for PaletteState {
    fn default() -> Self {
        Self { mode: Mode::Main, inner: egui_palette::State::default() }
    }
}

impl PaletteState {
    pub fn open_at(&mut self, mode: Mode) {
        self.mode = mode;
        self.inner.open();
    }

    pub fn close(&mut self) {
        self.inner.close();
    }

    pub fn is_open(&self) -> bool {
        self.inner.open
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Everything -- commands + files + the `Run Template...` entry
    /// that cascades into [`Mode::Templates`].
    Main,
    /// Second-level cascade shown after the user picks `Run Template...`
    /// from the main list. Registered templates + an install entry.
    Templates,
    /// Third-level cascade: list installed templates to remove.
    /// Picking one deletes its `.bt` file (and any siblings we added
    /// for it). Reached from `Main` via "Uninstall template...".
    Uninstall,
    /// Recent filesystem files picked from `AppSettings::recent_files`,
    /// already-open paths filtered out. Reached from `Main` via
    /// "Open recent...".
    Recent,
    /// Prompt for a single offset (absolute, or +/- relative to the
    /// current cursor). Enter jumps the caret.
    GoToOffset,
    /// Prompt for a byte count starting at the current cursor.
    SelectFromOffset,
    /// Prompt for a `<start>, <end>` (or `<start>..<end>`) range.
    SelectRange,
}

impl Mode {
    /// One level up the cascade, or `None` if already at the root.
    /// Used by the Escape-pops-back behaviour in
    /// `apply_palette_action`. All sub-modes today are reached
    /// directly from `Main`, so this collapses to a single `Main`
    /// parent for everything except `Main` itself.
    pub fn parent(self) -> Option<Self> {
        match self {
            Mode::Main => None,
            Mode::Templates
            | Mode::Uninstall
            | Mode::Recent
            | Mode::GoToOffset
            | Mode::SelectFromOffset
            | Mode::SelectRange => Some(Mode::Main),
        }
    }
}

/// Activation payload the app hands back to itself when the user
/// picks an entry. Cloneable so it can ride back through
/// [`egui_palette::Outcome::Picked`].
/// Enumerated palette commands. Replaces the previous string-keyed
/// [`Action::InvokeCommand`] payload so the dispatch in
/// `apply_palette_action` is exhaustive at compile time -- typos
/// and unused entries turn into `match` errors instead of silent
/// no-ops through a `_ => {}` arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteCommand {
    NewFile,
    OpenFile,
    BrowseArchive,
    ToggleConsole,
    ToggleInspector,
    TogglePlugins,
    Undo,
    Redo,
    Paste,
    PasteAsHex,
    SplitRight,
    SplitLeft,
    SplitUp,
    SplitDown,
    MergeRight,
    MergeLeft,
    MergeUp,
    MergeDown,
    ToggleEditMode,
    /// Copy the active tab's caret offset as a formatted number.
    CopyCaretOffset,
    /// Copy `start-end (N bytes)` for the active tab's non-empty
    /// selection.
    CopySelectionRange,
    /// Copy just the selection length (the number of bytes it spans).
    CopySelectionLength,
    /// Copy the active tab's total source length.
    CopyFileLength,
}

#[derive(Clone)]
pub enum Action {
    InvokeCommand(PaletteCommand),
    FocusFile(FileId),
    RunTemplate(PathBuf),
    SwitchMode(Mode),
    InstallTemplate,
    /// Delete the given `.bt` from the user's templates directory.
    UninstallTemplate(PathBuf),
    /// Copy the active file's current selection using the given
    /// format. Only offered when a non-empty selection exists.
    Copy(crate::copy_format::CopyKind),
    /// Open a previously-visited filesystem file by path. Used by
    /// the `Open recent` cascade mode.
    OpenRecent(PathBuf),
    /// Move the caret of the active tab to an absolute offset.
    /// Relative inputs (`+N`, `-N`) are resolved against the
    /// current cursor before the action is emitted, so the payload
    /// is always absolute.
    GoToOffset(u64),
    /// Replace the active tab's selection with `[start, end_exclusive)`.
    /// Covers both "Select N bytes from cursor" and "Select range".
    SetSelection { start: u64, end_exclusive: u64 },
    /// Intentionally does nothing. Used for non-actionable
    /// placeholder rows (e.g. "Invalid: ..." in the Go-To cascade
    /// when the query doesn't parse).
    NoOp,
}

/// Render the palette and return an outcome if the user activated
/// something or dismissed the panel this frame.
pub fn show(
    ctx: &egui::Context,
    state: &mut PaletteState,
    entries: Vec<egui_palette::Entry<Action>>,
) -> Option<Outcome> {
    let hint: String = match state.mode {
        Mode::Main => hxy_i18n::t("palette-hint-main"),
        Mode::Templates => hxy_i18n::t("palette-hint-templates"),
        Mode::Uninstall => hxy_i18n::t("palette-hint-uninstall"),
        Mode::Recent => hxy_i18n::t("palette-hint-recent"),
        Mode::GoToOffset => hxy_i18n::t("palette-hint-go-to-offset"),
        Mode::SelectFromOffset => hxy_i18n::t("palette-hint-select-from-offset"),
        Mode::SelectRange => hxy_i18n::t("palette-hint-select-range"),
    };
    // Argument-style modes build a single dynamic entry from the
    // query itself; fuzzy-filtering that entry against the raw
    // argument text would hide it the moment the argument isn't a
    // subsequence of the human-readable row label (e.g. hex args
    // with `0x` + commas that don't appear in the `Select .. ..`
    // output format).
    state.inner.bypass_filter = matches!(state.mode, Mode::GoToOffset | Mode::SelectFromOffset | Mode::SelectRange);
    match egui_palette::show(ctx, &mut state.inner, &entries, &hint)? {
        egui_palette::Outcome::Dismissed(reason) => Some(Outcome::Dismissed(reason)),
        egui_palette::Outcome::Picked(action) => Some(Outcome::Picked(action)),
    }
}

pub use egui_palette::DismissReason;

pub enum Outcome {
    Picked(Action),
    /// User dismissed without picking. The reason lets the host
    /// pop a cascade level on Escape but always close on backdrop
    /// click (see `app.rs` `handle_command_palette` dispatch).
    Dismissed(DismissReason),
}
