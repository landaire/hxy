//! Per-tab open-file state.

use std::path::PathBuf;
use std::sync::Arc;

use hxy_core::ByteOffset;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_core::Selection;
use hxy_vfs::MountedVfs;
use hxy_vfs::TabSource;
use hxy_vfs::VfsHandler;
use thiserror::Error;

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

pub struct OpenFile {
    pub id: FileId,
    pub display_name: String,
    /// Persistent identity of the tab's byte source. `None` for
    /// temporary in-memory buffers that shouldn't survive a restart.
    pub source_kind: Option<TabSource>,
    pub source: Arc<dyn HexSource>,
    pub selection: Option<Selection>,
    /// Last-hovered byte offset reported by the hex view -- surfaced in
    /// the status bar. Cleared each frame (set from `HexViewResponse`).
    pub hovered: Option<ByteOffset>,
    /// Most recent scroll offset reported by the hex view.
    pub scroll_offset: f32,
    /// When `Some`, the widget should scroll to this offset on its next
    /// frame. Used to restore saved scroll position on reopen. Cleared
    /// after one frame so the user can scroll freely afterward.
    pub pending_scroll: Option<f32>,
    /// Programmatic "scroll to this byte" request. Resolved at render
    /// time (needs columns + row height). Takes precedence over
    /// `pending_scroll`. Cleared after one frame.
    pub pending_scroll_to_byte: Option<hxy_core::ByteOffset>,
    /// VFS handler detected for this file's byte source, if any. Cached
    /// from the first-frame detection so the toolbar command can check
    /// availability without re-scanning on each frame.
    pub detected_handler: Option<Arc<dyn VfsHandler>>,
    /// Mounted VFS, if the user has opened the archive via the
    /// "Browse archive" command. Shared so descendant tabs can open
    /// entries against the same mount.
    pub mount: Option<Arc<MountedVfs>>,
    /// Whether the VFS tree side panel should render for this tab. Only
    /// meaningful when `mount` is `Some`. Starts true on mount; the
    /// user can hide the panel via its close button.
    pub show_vfs_tree: bool,
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
    /// these to paint each field's bytes a distinct colour when
    /// [`Self::show_colors`] is on.
    pub leaf_colors: Vec<egui::Color32>,
    /// When true, the hex view recolours bytes by their containing
    /// template field. Toggled from the template panel header.
    pub show_colors: bool,
    /// Plugin-supplied per-byte palette (one colour per value 0..=255),
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
        Self {
            id,
            display_name: display_name.into(),
            source_kind,
            source: Arc::new(MemorySource::new(bytes)),
            selection: None,
            hovered: None,
            scroll_offset: 0.0,
            pending_scroll: None,
            pending_scroll_to_byte: None,
            detected_handler: None,
            mount: None,
            show_vfs_tree: false,
            #[cfg(not(target_arch = "wasm32"))]
            template: None,
            #[cfg(not(target_arch = "wasm32"))]
            template_running: None,
            #[cfg(not(target_arch = "wasm32"))]
            suggested_template: None,
        }
    }

    /// Convenience: the filesystem path this tab (or any ancestor tab)
    /// ultimately originates from. `None` only for purely in-memory
    /// tabs with no path backing (e.g. placeholder buffers).
    pub fn root_path(&self) -> Option<&PathBuf> {
        self.source_kind.as_ref().map(|s| s.root_path())
    }
}

#[derive(Debug, Error)]
pub enum FileOpenError {
    #[error("user cancelled the file picker")]
    Cancelled,
    #[error("read file {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}
