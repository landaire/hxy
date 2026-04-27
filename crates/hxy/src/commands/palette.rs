//! Thin hxy-specific wrapper around [`egui_palette`].
//!
//! The generic crate handles rendering, fuzzy-matching, and keyboard
//! nav. This module defines hxy's entry / action vocabulary and the
//! cascade mode (Main -> Templates -> ...).

#![cfg(not(target_arch = "wasm32"))]

use std::path::PathBuf;

use crate::files::FileId;

/// Persistent state for the palette. Keeps both the hxy-specific
/// cascade [`Mode`] and the generic [`egui_palette::State`] that
/// owns query / selection / focus.
pub struct PaletteState {
    pub mode: Mode,
    pub inner: egui_palette::State,
    /// Active plugin-driven sub-menu when `mode == Mode::PluginCascade`.
    /// Carries the plugin name (so further `invoke` calls route to
    /// the right handler) and the command list the plugin handed
    /// us, used to populate the sub-palette without re-asking the
    /// plugin every frame.
    pub plugin_cascade: Option<PluginCascadeState>,
    /// Active plugin prompt when `mode == Mode::PluginPrompt`.
    /// Carries the plugin name + originating command id (so the
    /// answer is routed back via `respond_to_prompt`) and the
    /// title rendered as the palette hint.
    pub plugin_prompt: Option<PluginPromptState>,
    /// In-progress compare-files pick when `mode` is one of the
    /// `Compare*` variants. Holds the A side once the user has
    /// chosen it so the B-pick mode can filter A out of its
    /// own lists.
    pub compare_pick: Option<ComparePickState>,
}

#[derive(Clone)]
pub struct PluginCascadeState {
    pub plugin_name: String,
    pub commands: Vec<hxy_plugin_host::PluginCommand>,
}

#[derive(Clone)]
pub struct PluginPromptState {
    pub plugin_name: String,
    pub command_id: String,
    pub title: String,
}

impl Default for PaletteState {
    fn default() -> Self {
        Self {
            mode: Mode::Main,
            inner: egui_palette::State::default(),
            plugin_cascade: None,
            plugin_prompt: None,
            compare_pick: None,
        }
    }
}

impl PaletteState {
    pub fn open_at(&mut self, mode: Mode) {
        self.mode = mode;
        // Cleared on every mode switch -- the only entry paths
        // into populated plugin buffers are `enter_plugin_cascade`
        // / `enter_plugin_prompt`, so any other transition should
        // drop them. Compare-pick state survives only across the
        // CompareSideA -> CompareSideB sequence and is cleared by
        // any transition outside that family.
        self.plugin_cascade = None;
        self.plugin_prompt = None;
        if !matches!(
            mode,
            Mode::CompareSideA | Mode::CompareSideARecent | Mode::CompareSideB | Mode::CompareSideBRecent
        ) {
            self.compare_pick = None;
        }
        self.inner.open();
    }

    pub fn close(&mut self) {
        self.inner.close();
        self.plugin_cascade = None;
        self.plugin_prompt = None;
        self.compare_pick = None;
    }

    pub fn is_open(&self) -> bool {
        self.inner.open
    }

    /// Push a fresh plugin-driven sub-menu and switch into the
    /// cascade mode. Resets the query / selection so the new entry
    /// list is searchable from scratch.
    pub fn enter_plugin_cascade(&mut self, plugin_name: String, commands: Vec<hxy_plugin_host::PluginCommand>) {
        self.plugin_cascade = Some(PluginCascadeState { plugin_name, commands });
        self.plugin_prompt = None;
        self.mode = Mode::PluginCascade;
        // open() resets query / selection / pending_focus and is
        // safe to call when already open -- mirrors how mode
        // switches like Templates / Recent are entered.
        self.inner.open();
    }

    /// Switch into argument-style prompt mode for a plugin's
    /// pending question. The user's typed answer is routed back
    /// via `Action::RespondToPlugin` carrying the same plugin
    /// name + command id stored here. `default_value` pre-fills
    /// the input so an "edit existing" flow can start from the
    /// last value.
    pub fn enter_plugin_prompt(
        &mut self,
        plugin_name: String,
        command_id: String,
        title: String,
        default_value: Option<String>,
    ) {
        self.plugin_prompt = Some(PluginPromptState { plugin_name, command_id, title });
        self.plugin_cascade = None;
        self.mode = Mode::PluginPrompt;
        self.inner.open();
        if let Some(value) = default_value {
            self.inner.query = value;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Everything -- commands + files + the `Run Template...` entry
    /// that cascades into [`Mode::Templates`]. Reached via the
    /// "command palette" shortcut (Cmd+Shift+P).
    Main,
    /// Filename-first quick-open list: just the currently-open
    /// files (in `app.files`) plus recent paths the user can
    /// re-open. No commands. Reached via the "quick open"
    /// shortcut (Cmd+P) and intended for fast tab switching.
    QuickOpen,
    /// Second-level cascade shown after the user picks `Run Template...`
    /// from the main list. Registered templates + an install entry.
    Templates,
    /// Third-level cascade: list installed templates to remove.
    /// Picking one deletes its `.bt` file (and any siblings we added
    /// for it). Reached from `Main` via "Uninstall template...".
    Uninstall,
    /// Third-level cascade: list installed WASM plugins to remove.
    /// Picking one deletes the `.wasm` + `.hxy.toml` sidecar, drops
    /// the user's stored grant, and clears persisted plugin state.
    /// Reached from `Main` via "Uninstall plugin...".
    UninstallPlugin,
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
    /// Prompt for a hex-view column count to apply to the active
    /// buffer only. The value overrides
    /// `AppSettings::hex_columns` for that tab without touching the
    /// global default.
    SetColumnsLocal,
    /// Prompt for a hex-view column count to apply globally
    /// (overwrites `AppSettings::hex_columns`). Existing per-tab
    /// overrides from [`Mode::SetColumnsLocal`] continue to win for
    /// their own tabs.
    SetColumnsGlobal,
    /// Sub-palette populated by a plugin's `Cascade` invoke
    /// outcome. The actual entry list lives on
    /// [`PaletteState::plugin_cascade`] -- this variant is just
    /// the marker that build-entries should read from there.
    PluginCascade,
    /// Argument-style prompt raised by a plugin's `Prompt` invoke
    /// outcome. The plugin's title becomes the palette hint, the
    /// user's typed answer is dispatched back via
    /// [`Action::RespondToPlugin`]. Context (plugin name + command
    /// id + title) lives on [`PaletteState::plugin_prompt`].
    PluginPrompt,
    /// First step of the palette-driven file comparison: pick
    /// the A side. Top-level entries are open files plus
    /// "Recent files..." / "Browse..." cascades.
    CompareSideA,
    /// Cascade reached from [`Mode::CompareSideA`]'s
    /// "Recent files..." entry. Picking a recent path completes
    /// the A pick and advances to [`Mode::CompareSideB`].
    CompareSideARecent,
    /// Pick the B side. The chosen A is on
    /// [`PaletteState::compare_pick`] so we can filter it out of
    /// this list. Same cascades as `CompareSideA`.
    CompareSideB,
    /// Recent-files cascade for the B pick.
    CompareSideBRecent,
}

/// State carried while the palette is walking the user through
/// the compare-files state machine. `picked_a` is `Some` once the
/// user has chosen the A side; the B-pick modes use it to filter
/// A out of their own lists.
#[derive(Clone, Debug)]
pub struct ComparePickState {
    pub picked_a: Option<hxy_vfs::TabSource>,
}

/// Which scope a [`Mode::SetColumnsLocal`] / [`Mode::SetColumnsGlobal`]
/// pick should write its column count to. Carried by
/// [`Action::SetColumns`] so dispatch in `apply_palette_action` can
/// fan out to either the active `OpenFile` or the persisted
/// `AppSettings`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnScope {
    Local,
    Global,
}

impl Mode {
    /// One level up the cascade, or `None` if already at the root.
    /// Used by the Escape-pops-back behaviour in
    /// `apply_palette_action`. All sub-modes today are reached
    /// directly from `Main`, so this collapses to a single `Main`
    /// parent for everything except `Main` itself.
    pub fn parent(self) -> Option<Self> {
        match self {
            // Top-level modes -- Escape closes the palette outright
            // rather than popping back somewhere.
            Mode::Main | Mode::QuickOpen => None,
            Mode::Templates
            | Mode::Uninstall
            | Mode::UninstallPlugin
            | Mode::Recent
            | Mode::GoToOffset
            | Mode::SelectFromOffset
            | Mode::SelectRange
            | Mode::SetColumnsLocal
            | Mode::SetColumnsGlobal
            | Mode::PluginCascade
            | Mode::PluginPrompt
            | Mode::CompareSideA => Some(Mode::Main),
            Mode::CompareSideARecent => Some(Mode::CompareSideA),
            Mode::CompareSideB => Some(Mode::CompareSideA),
            Mode::CompareSideBRecent => Some(Mode::CompareSideB),
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
    BrowseVfs,
    /// Toggle the workspace VFS tree on/off without losing the
    /// workspace itself. When the active tab isn't a workspace, the
    /// command is a no-op.
    ToggleWorkspaceVfs,
    /// Toggle the right-hand tool panel (Plugins manager + plugin
    /// mount tabs). Hides every tool-class tab into a stash; toggling
    /// again re-creates the panel and restores them.
    ToggleToolPanel,
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
    /// Move just the focused tab into the neighbour pane in the
    /// given direction. Distinct from the `Merge*` commands, which
    /// pull every tab in the leaf along.
    MoveTabRight,
    MoveTabLeft,
    MoveTabUp,
    MoveTabDown,
    /// Visual pickers: close the palette and overlay each candidate
    /// pane with a single uppercase letter; pressing the matching
    /// letter executes the move/merge against that pane. See
    /// `pane_pick` module.
    MoveTabVisual,
    MergeVisual,
    /// Visual focus picker: like the move/merge variants but
    /// sourceless -- every leaf in the dock gets a letter, and
    /// pressing one moves keyboard focus + active-tab to that leaf.
    FocusPane,
    /// Toggle Vim-style modal editing on the active tab and the
    /// global default for newly-opened tabs. Off (default) restores
    /// the standard arrow-key dispatcher.
    ToggleVim,
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
    /// Drive the file-comparison flow through the command palette
    /// itself: the user picks A from open files / recents / browse,
    /// then B (with A filtered out), and a `Tab::Compare` opens.
    /// Entry point that switches into [`Mode::CompareSideA`].
    CompareFiles,
    /// Same outcome as `CompareFiles`, but routed through the modal
    /// picker dialog. Kept for users who prefer the side-by-side
    /// comboboxes.
    CompareFilesDialog,
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
    /// Uninstall the WASM plugin whose component lives at `wasm_path`.
    /// The dispatcher deletes the `.wasm` + sidecar, drops the
    /// stored grant for the plugin's `PluginKey`, and clears any
    /// persisted blob the plugin owned. Triggers a plugin rescan
    /// so the change is reflected immediately.
    UninstallPlugin(PathBuf),
    /// Copy the active file's current selection using the given
    /// format. Only offered when a non-empty selection exists.
    Copy(crate::files::copy::CopyKind),
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
    SetSelection {
        start: u64,
        end_exclusive: u64,
    },
    /// Set the hex view's column count, either for the active tab
    /// (`Local`) or the global default (`Global`). Validated to a
    /// non-zero `u16` before the action is emitted, so the dispatch
    /// can take the value verbatim.
    SetColumns {
        scope: ColumnScope,
        count: hxy_core::ColumnCount,
    },
    /// Activate a plugin-contributed palette command. The dispatcher
    /// looks the plugin up by `plugin_name` (matched against
    /// `PluginHandler::name()`), forwards `command_id` through
    /// `PluginHandler::invoke_command`, and acts on the returned
    /// outcome. `plugin_name` is captured at entry-build time so
    /// the action stays self-contained even if the plugin list
    /// reshuffles before activation.
    InvokePluginCommand {
        plugin_name: String,
        command_id: String,
    },
    /// Submit the user's typed answer to a previously-emitted
    /// plugin prompt. Routed through
    /// `PluginHandler::respond_to_prompt`; the resulting
    /// [`hxy_plugin_host::InvokeOutcome`] decides what happens
    /// next (close / cascade / mount / chain another prompt).
    RespondToPlugin {
        plugin_name: String,
        command_id: String,
        answer: String,
    },
    /// Intentionally does nothing. Used for non-actionable
    /// placeholder rows (e.g. "Invalid: ..." in the Go-To cascade
    /// when the query doesn't parse).
    NoOp,
    /// User picked a source for the compare A or B side. The
    /// dispatcher captures A and switches into B-mode; on B it
    /// finalises by spawning a [`crate::compare::CompareSession`]
    /// and closes the palette.
    CompareSelectSource {
        side: CompareSide,
        source: hxy_vfs::TabSource,
    },
    /// Open the OS file dialog for the indicated side; whatever
    /// the user picks routes back through
    /// `Action::CompareSelectSource`.
    CompareBrowse(CompareSide),
}

/// Which side of a compare pick a palette entry contributes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareSide {
    A,
    B,
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
        Mode::QuickOpen => hxy_i18n::t("palette-hint-quick-open"),
        Mode::Templates => hxy_i18n::t("palette-hint-templates"),
        Mode::Uninstall => hxy_i18n::t("palette-hint-uninstall"),
        Mode::UninstallPlugin => hxy_i18n::t("palette-hint-uninstall-plugin"),
        Mode::Recent => hxy_i18n::t("palette-hint-recent"),
        Mode::CompareSideA => hxy_i18n::t("palette-hint-compare-side-a"),
        Mode::CompareSideARecent | Mode::CompareSideBRecent => hxy_i18n::t("palette-hint-recent"),
        Mode::CompareSideB => hxy_i18n::t("palette-hint-compare-side-b"),
        Mode::GoToOffset => hxy_i18n::t("palette-hint-go-to-offset"),
        Mode::SelectFromOffset => hxy_i18n::t("palette-hint-select-from-offset"),
        Mode::SelectRange => hxy_i18n::t("palette-hint-select-range"),
        Mode::SetColumnsLocal => hxy_i18n::t("palette-hint-set-columns-local"),
        Mode::SetColumnsGlobal => hxy_i18n::t("palette-hint-set-columns-global"),
        Mode::PluginCascade => hxy_i18n::t("palette-hint-main"),
        // The plugin authored the prompt's wording -- pass it
        // through verbatim. Falls back to a generic hint if the
        // mode was reached without setting up the prompt buffer
        // (shouldn't happen in practice; keeps the match total).
        Mode::PluginPrompt => {
            state.plugin_prompt.as_ref().map(|p| p.title.clone()).unwrap_or_else(|| hxy_i18n::t("palette-hint-main"))
        }
    };
    // Argument-style modes build a single dynamic entry from the
    // query itself; fuzzy-filtering that entry against the raw
    // argument text would hide it the moment the argument isn't a
    // subsequence of the human-readable row label (e.g. hex args
    // with `0x` + commas that don't appear in the `Select .. ..`
    // output format).
    state.inner.bypass_filter = matches!(
        state.mode,
        Mode::GoToOffset
            | Mode::SelectFromOffset
            | Mode::SelectRange
            | Mode::SetColumnsLocal
            | Mode::SetColumnsGlobal
            | Mode::PluginPrompt
    );
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
