//! Deferred dock / file effects shared by the various command
//! dispatchers.
//!
//! The toolbar that originally owned this enum is gone -- the command
//! palette is the only entry point now. The enum stays because both
//! the palette and the macOS native menu funnel through
//! `apply_command_effect` in `app.rs`, which is cleaner than each
//! call site reaching directly into the dock-mutation helpers.

#[derive(Debug)]
pub enum CommandEffect {
    OpenFileDialog,
    MountActiveFile,
    OpenRecent(std::path::PathBuf),
    RunTemplateDialog,
    /// Run a specific template file without prompting -- pushed when
    /// the auto-detected library has matched the active file.
    RunTemplateDirect(std::path::PathBuf),
    /// Undo the most recent edit on the active tab.
    UndoActiveFile,
    /// Redo the most recently undone edit on the active tab.
    RedoActiveFile,
    /// Split the focused dock leaf, duplicating the active tab into
    /// the new pane.
    DockSplit(DockDir),
    /// Merge the focused dock leaf with the neighbour in `DockDir`,
    /// moving all its tabs into that neighbour and collapsing the
    /// empty split.
    DockMerge(DockDir),
    /// Move just the focused tab into the neighbour leaf in
    /// `DockDir`. Distinct from [`Self::DockMerge`], which carries
    /// every tab in the leaf along; this only relocates the active
    /// tab, leaving siblings put. If the source leaf is left empty
    /// it gets collapsed the same way merge does.
    DockMoveTab(DockDir),
}

/// Directional axis for dock-pane split / merge / move-tab commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockDir {
    Left,
    Right,
    Up,
    Down,
}
