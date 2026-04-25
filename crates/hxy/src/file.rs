//! Per-tab open-file state.
//!
//! The editor-specific bits (source + patch overlay, selection,
//! scroll, undo/redo, keystroke routing) live on
//! [`hxy_view::HexEditor`]; `OpenFile` composes one and layers on
//! the app-level metadata the tab needs (display name, filesystem
//! source kind, VFS mount, template run state, etc.).

use std::path::PathBuf;
use std::sync::Arc;

use hxy_core::ByteOffset;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_vfs::MountedVfs;
use hxy_vfs::TabSource;
use hxy_vfs::VfsHandler;

pub use hxy_view::EditEntry;
pub use hxy_view::EditMode;
pub use hxy_view::WriteError;

/// Identifier for an open-file tab. Stable across the tab's lifetime so
/// egui_dock can refer to it even as the tab moves around the dock tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileId(u64);

impl FileId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Identifier for an active plugin VFS mount. Distinct from `FileId`
/// because plugin mounts are not file tabs -- they own a `MountedVfs`
/// and render only the VFS tree. Children opened from the tree become
/// regular `FileId` tabs whose `mount` field shares the same Arc.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MountId(u64);

#[cfg(not(target_arch = "wasm32"))]
impl MountId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }
    pub fn get(self) -> u64 {
        self.0
    }
}

/// A plugin mount the user has opened. Backs a `Tab::PluginMount` --
/// the tab renders the VFS tree and clicking an entry opens a regular
/// `Tab::File` whose bytes come from this mount.
#[cfg(not(target_arch = "wasm32"))]
pub struct MountedPlugin {
    pub display_name: String,
    pub plugin_name: String,
    pub token: String,
    pub mount: Arc<MountedVfs>,
}

/// Identifier for a `Workspace` -- the nested-dock representation of a
/// file with a live VFS handler. Each workspace owns one parent
/// `OpenFile` (the underlying file) plus a `MountedVfs` plus an inner
/// `DockState<WorkspaceTab>` whose tabs can include the editor, the
/// VFS tree, and any number of opened entries. Available on every
/// target -- the built-in ZIP handler runs in the browser too.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkspaceId(u64);

impl WorkspaceId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Sub-tabs rendered inside a `Tab::Workspace`'s nested dock area.
/// `Editor` and `VfsTree` are workspace-scoped singletons; multiple
/// `Entry` tabs can coexist (one per opened VFS entry).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum WorkspaceTab {
    /// The workspace's underlying file (a regular hex view).
    Editor,
    /// The mount's VFS tree. The user can move it anywhere within
    /// the workspace's dock or close it.
    VfsTree,
    /// A file entry opened from the VFS tree. Carries the FileId of
    /// the entry's `OpenFile`, which the host stores in `app.files`
    /// the same way it does for top-level file tabs.
    Entry(FileId),
}

/// A file-rooted VFS workspace. Owns the file's identity, the mount,
/// and the inner dock that arranges the editor + tree + opened entries.
pub struct Workspace {
    pub id: WorkspaceId,
    /// FileId of the workspace's underlying file. The `OpenFile` lives
    /// in `app.files`, same as for any top-level file tab.
    pub editor_id: FileId,
    pub mount: Arc<MountedVfs>,
    pub dock: egui_dock::DockState<WorkspaceTab>,
}

impl Workspace {
    /// Default layout: editor on the right, VFS tree as a left split
    /// at ~30% width. Matches what the previous side-panel layout
    /// looked like; the user can re-dock as they please.
    pub fn new(id: WorkspaceId, editor_id: FileId, mount: Arc<MountedVfs>) -> Self {
        let mut dock = egui_dock::DockState::new(vec![WorkspaceTab::Editor]);
        dock.main_surface_mut().split_left(
            egui_dock::NodeIndex::root(),
            0.3,
            vec![WorkspaceTab::VfsTree],
        );
        Self { id, editor_id, mount, dock }
    }
}

pub struct OpenFile {
    pub id: FileId,
    pub display_name: String,
    /// Persistent identity of the tab's byte source. `None` for
    /// temporary in-memory buffers that shouldn't survive a restart.
    pub source_kind: Option<TabSource>,
    /// All editor-visible state: byte source, patch overlay,
    /// selection, scroll, undo/redo, active pane, edit mode.
    pub editor: hxy_view::HexEditor,
    /// Last-hovered byte offset reported by the hex view -- surfaced in
    /// the status bar. Cleared each frame (set from the render
    /// response).
    pub hovered: Option<ByteOffset>,
    /// VFS handler detected for this file's byte source, if any. Cached
    /// from the first-frame detection so the toolbar command can check
    /// availability without re-scanning on each frame. When the user
    /// invokes "Browse VFS" the host wraps this file in a `Workspace`
    /// (see `HxyApp::workspaces`) using `detected_handler` to construct
    /// the mount; `OpenFile` itself does not own a mount.
    pub detected_handler: Option<Arc<dyn VfsHandler>>,
    /// Template run state for this tab, if the user has applied a
    /// template. `None` until the first successful run.
    #[cfg(not(target_arch = "wasm32"))]
    pub template: Option<TemplateState>,
    /// Background parse+execute in flight. Mutually exclusive with
    /// `template` in practice -- when a run starts we clear the old
    /// tree; when the run finishes we swap the result in here.
    #[cfg(not(target_arch = "wasm32"))]
    pub template_running: Option<TemplateRun>,
    /// Template auto-detected for this file by the library scanner:
    /// either a File Mask extension hit or an ID Bytes magic match.
    /// `None` when no library entry matches.
    #[cfg(not(target_arch = "wasm32"))]
    pub suggested_template: Option<SuggestedTemplate>,
    /// Per-tab override for the hex view's column count. `None` means
    /// follow the global `AppSettings::hex_columns` default; `Some`
    /// pins this buffer to a specific width regardless of the global
    /// setting. Set via the `Set hex columns (this buffer)` palette
    /// command and not currently persisted across restarts.
    pub hex_columns_override: Option<hxy_core::ColumnCount>,
    /// Per-tab search bar state. Live as long as the tab; not
    /// persisted across restarts. The bar visibility flag lives on the
    /// state itself rather than a separate boolean so reopening the
    /// bar restores the user's last query.
    pub search: crate::search::SearchState,
}

/// A template library entry pre-matched against a file's first bytes
/// and extension. Stored on the tab so the toolbar can render its
/// label (`Run ZIP.bt`) and invoke the runtime without re-scanning.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug)]
pub struct SuggestedTemplate {
    pub path: PathBuf,
    pub display_name: String,
}

/// In-flight template run on a worker thread. Receives the full
/// parse+execute result via an [`egui_inbox::UiInbox`]; sending into
/// the inbox triggers a repaint automatically, so the UI picks up
/// the result on the very next frame.
#[cfg(not(target_arch = "wasm32"))]
pub struct TemplateRun {
    pub inbox: egui_inbox::UiInbox<TemplateRunOutcome>,
    pub template_name: String,
    pub started: jiff::Timestamp,
}

#[cfg(not(target_arch = "wasm32"))]
pub enum TemplateRunOutcome {
    Ok { parsed: std::sync::Arc<dyn hxy_plugin_host::ParsedTemplate>, tree: hxy_plugin_host::template::ResultTree },
    Err(String),
}

/// Result of applying a template-language runtime to the tab's byte
/// source. Holds the parsed template (so deferred arrays can be
/// expanded lazily) and the current tree view state.
/// Index into a [`TemplateState::tree`]'s flat node list. Newtype so
/// we don't confuse it with the `u64` array ids the runtime hands out
/// for deferred arrays.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TemplateNodeIdx(pub u32);

/// Opaque identifier for a deferred array, handed back to the plugin
/// when the UI wants to materialise more elements. Distinct from
/// [`TemplateNodeIdx`] -- same `u64` width as the WIT record but
/// typed so we can't pass a node index where an array id is wanted.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TemplateArrayId(pub u64);

#[cfg(not(target_arch = "wasm32"))]
pub struct TemplateState {
    /// `None` when the state was built as a diagnostics-only surface
    /// (e.g. missing runtime, parse failure) -- in that case
    /// `expand_array` can't be called and the panel renders only the
    /// diagnostics header.
    pub parsed: Option<std::sync::Arc<dyn hxy_plugin_host::ParsedTemplate>>,
    pub tree: hxy_plugin_host::template::ResultTree,
    /// Show the panel in the file tab. User can toggle via the tree
    /// panel's close button.
    pub show_panel: bool,
    /// Array id -> materialised children, by order of expansion.
    pub expanded_arrays: std::collections::HashMap<TemplateArrayId, Vec<hxy_plugin_host::template::Node>>,
    /// Indexes of nodes whose subtrees the user has collapsed. Default
    /// is expanded; we store the negation so freshly-run templates
    /// reveal everything without per-node defaults.
    pub collapsed: std::collections::HashSet<TemplateNodeIdx>,
    /// Last-frame's hover target in the panel table: the node index
    /// whose row the pointer is over, if any. Consumed by the hex
    /// view to paint a highlight over that node's byte span.
    pub hovered_node: Option<TemplateNodeIdx>,
    /// Precomputed (offset, length) spans for every leaf node in
    /// the tree, sorted by offset. Passed to `HexView` so it can
    /// draw field-boundary outlines without walking the tree each
    /// frame.
    pub leaf_boundaries: Vec<(hxy_core::ByteOffset, hxy_core::ByteLen)>,
    /// One tint per entry in `leaf_boundaries`. The hex view uses
    /// these to paint each field's bytes a distinct color when
    /// [`Self::show_colors`] is on.
    pub leaf_colors: Vec<egui::Color32>,
    /// When true, the hex view recolors bytes by their containing
    /// template field. Toggled from the template panel header.
    pub show_colors: bool,
    /// Plugin-supplied per-byte palette (one color per value 0..=255),
    /// extracted once from the runtime's `ResultTree::byte_palette`.
    /// When `Some`, overrides the user's byte-value highlight for
    /// this tab.
    pub byte_palette_override: Option<std::sync::Arc<[egui::Color32; 256]>>,
}

impl OpenFile {
    /// Construct from an in-memory buffer -- used for initial load of
    /// small files before we have a streaming reader.
    pub fn from_bytes(
        id: FileId,
        display_name: impl Into<String>,
        source_kind: Option<TabSource>,
        bytes: Vec<u8>,
    ) -> Self {
        let base: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
        Self::from_source(id, display_name, source_kind, base)
    }

    /// Pick an appropriate default [`EditMode`] for a file-backed tab.
    /// Writable on-disk files default to `Mutable`; a file whose
    /// permissions forbid writing (or whose metadata we can't read)
    /// defaults to `Readonly`. Callers with no filesystem source
    /// (pure in-memory buffers, VFS entries) should just default to
    /// `Mutable` directly.
    pub fn default_mode_for_path(path: &std::path::Path) -> EditMode {
        match std::fs::metadata(path) {
            Ok(meta) if !meta.permissions().readonly() => EditMode::Mutable,
            _ => EditMode::Readonly,
        }
    }

    /// Construct from any pre-built [`HexSource`]. Wraps it in a
    /// [`hxy_view::HexEditor`] whose patch overlay records future
    /// writes.
    pub fn from_source(
        id: FileId,
        display_name: impl Into<String>,
        source_kind: Option<TabSource>,
        base: Arc<dyn HexSource>,
    ) -> Self {
        let mut editor = hxy_view::HexEditor::new(base);
        // Default to mutable whenever we can actually write: pure
        // in-memory buffers always can, filesystem-backed tabs only
        // when the permissions allow. Users who want to explore
        // without touching bytes can still flip the lock.
        let edit_mode = match source_kind.as_ref() {
            Some(TabSource::Filesystem(path)) => Self::default_mode_for_path(path),
            _ => EditMode::Mutable,
        };
        editor.set_edit_mode(edit_mode);
        Self {
            id,
            display_name: display_name.into(),
            source_kind,
            editor,
            hovered: None,
            detected_handler: None,
            #[cfg(not(target_arch = "wasm32"))]
            template: None,
            #[cfg(not(target_arch = "wasm32"))]
            template_running: None,
            #[cfg(not(target_arch = "wasm32"))]
            suggested_template: None,
            hex_columns_override: None,
            search: crate::search::SearchState::default(),
        }
    }

    /// Convenience: the filesystem path this tab (or any ancestor tab)
    /// ultimately originates from. `None` only for purely in-memory
    /// tabs with no path backing (e.g. placeholder buffers).
    pub fn root_path(&self) -> Option<&PathBuf> {
        self.source_kind.as_ref().and_then(|s| s.root_path())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FileOpenError {
    #[error("user cancelled the file picker")]
    Cancelled,
    #[error("read file {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A persisted plugin-mounted tab couldn't be re-bound on
    /// startup, either because the plugin is no longer installed or
    /// because its `mount_by_token` rejected the saved token (e.g.
    /// the underlying connection / profile was wiped from the
    /// plugin's own state). The tab is dropped from `open_tabs`
    /// rather than left orphaned.
    #[error("restore plugin mount for {plugin_name:?} (token {token:?}): {reason}")]
    PluginMount {
        plugin_name: String,
        token: String,
        reason: String,
    },
}
