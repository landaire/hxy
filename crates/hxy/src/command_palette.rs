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
    };
    match egui_palette::show(ctx, &mut state.inner, &entries, &hint)? {
        egui_palette::Outcome::Closed => Some(Outcome::Closed),
        egui_palette::Outcome::Picked(action) => Some(Outcome::Picked(action)),
    }
}

pub enum Outcome {
    Picked(Action),
    Closed,
}
