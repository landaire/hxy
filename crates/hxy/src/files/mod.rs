//! Per-tab open-file state.
//!
//! The editor-specific bits (source + patch overlay, selection,
//! scroll, undo/redo, keystroke routing) live on
//! [`hxy_view::HexEditor`]; `OpenFile` composes one and layers on
//! the app-level metadata the tab needs (display name, filesystem
//! source kind, VFS mount, template run state, etc.).

#[cfg(not(target_arch = "wasm32"))]
pub mod copy;
#[cfg(not(target_arch = "wasm32"))]
pub mod new;
#[cfg(not(target_arch = "wasm32"))]
pub mod open;
#[cfg(not(target_arch = "wasm32"))]
pub mod paste;
#[cfg(not(target_arch = "wasm32"))]
pub mod patch_persist;
#[cfg(not(target_arch = "wasm32"))]
pub mod save;
#[cfg(not(target_arch = "wasm32"))]
pub mod snapshot;
#[cfg(not(target_arch = "wasm32"))]
pub mod snapshot_ui;
#[cfg(not(target_arch = "wasm32"))]
pub mod streaming;
#[cfg(not(target_arch = "wasm32"))]
pub mod watch;

use std::path::PathBuf;
use std::sync::Arc;

use hxy_core::Attribution;
use hxy_core::ByteCache;
use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::CachedSource;
use hxy_core::HexSource;
use hxy_core::HexViewKey;
use hxy_core::MemorySource;
use hxy_core::SourceId;
use hxy_vfs::MountedVfs;
use hxy_vfs::TabSource;
use hxy_vfs::VfsHandler;

pub use hxy_view::EditEntry;
pub use hxy_view::EditMode;
pub use hxy_view::WriteError;

/// A reason a buffer is hard-readonly: the user cannot toggle the
/// editor to mutable, and the lock icon's tooltip explains why.
/// Detected at open / restore time by inspecting the byte source's
/// backing mount; orthogonal to OS-level filesystem readonly, which
/// is a soft hint the user can still flip locally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadOnlyReason {
    /// The byte source is a `TabSource::VfsEntry` whose parent mount
    /// has no `VfsWriter` -- the underlying handler simply doesn't
    /// support in-place writes (e.g. zip, minidump).
    VfsNoWriter,
}

impl ReadOnlyReason {
    /// Fluent key for the human-readable reason text. Resolved via
    /// `hxy_i18n::t` at the call site so a locale change triggers a
    /// fresh lookup without the reason itself caching English.
    pub fn message_key(&self) -> &'static str {
        match self {
            Self::VfsNoWriter => "readonly-reason-vfs-no-writer",
        }
    }
}

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
/// the tab renders the VFS tree (or a failure placeholder, see
/// [`MountStatus`]) and clicking an entry opens a regular
/// `Tab::File` whose bytes come from the live mount.
#[cfg(not(target_arch = "wasm32"))]
pub struct MountedPlugin {
    pub display_name: String,
    pub plugin_name: String,
    pub token: String,
    pub status: MountStatus,
}

/// Whether a plugin mount is currently usable. Restored mounts may
/// arrive in [`MountStatus::Failed`] when remounting failed (xbox
/// kit offline, deleted profile, etc.); the host renders a
/// placeholder tab with the plugin's message and -- when
/// `retry_label` is `Some` -- a button that re-invokes
/// `mount_by_token` with the same token.
#[cfg(not(target_arch = "wasm32"))]
pub enum MountStatus {
    Ready(Arc<MountedVfs>),
    Failed { message: String, retry_label: Option<String> },
}

#[cfg(not(target_arch = "wasm32"))]
impl MountStatus {
    /// Live mount handle when ready, `None` when the mount couldn't
    /// be established. Use this rather than matching `Status` directly
    /// at every read site.
    pub fn live(&self) -> Option<&Arc<MountedVfs>> {
        match self {
            MountStatus::Ready(m) => Some(m),
            MountStatus::Failed { .. } => None,
        }
    }
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
        dock.main_surface_mut().split_left(egui_dock::NodeIndex::root(), 0.3, vec![WorkspaceTab::VfsTree]);
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
    /// Completed template runs for this tab, in the order the user
    /// kicked them off. Each instance carries the byte range it was
    /// applied to so multiple templates -- on overlapping or disjoint
    /// regions -- can coexist as separate tabs in the template panel.
    #[cfg(not(target_arch = "wasm32"))]
    pub templates: Vec<TemplateInstance>,
    /// In-flight parse+execute jobs. Each one will eventually land in
    /// [`Self::templates`] (success) or be replaced with a diagnostics-
    /// only error instance.
    #[cfg(not(target_arch = "wasm32"))]
    pub templates_running: Vec<TemplateRunInstance>,
    /// Which template tab is currently selected in the template panel.
    /// `None` when the file has no completed templates yet, or the user
    /// closed the panel and we haven't picked a new active id.
    #[cfg(not(target_arch = "wasm32"))]
    pub active_template: Option<TemplateInstanceId>,
    /// Counter for handing out fresh template instance ids on this tab.
    /// Scoped per-file because instance ids only need to be unique
    /// within a single tab's [`Self::templates`] list (egui_table tab
    /// keys, palette routing, etc.).
    #[cfg(not(target_arch = "wasm32"))]
    pub next_template_instance_id: u64,
    /// Whether the per-file template panel is visible. Hidden by the
    /// panel's close button; set back to `true` whenever a new
    /// template run lands. Spans across tabs because the panel itself
    /// is one widget; per-instance visibility is expressed by which
    /// tab is active.
    #[cfg(not(target_arch = "wasm32"))]
    pub template_panel_visible: bool,
    /// Template auto-detected for this file by the library scanner:
    /// either a File Mask extension hit or an ID Bytes magic match.
    /// `None` when no library entry matches.
    #[cfg(not(target_arch = "wasm32"))]
    pub suggested_template: Option<SuggestedTemplate>,
    /// Path to the template file most recently run against this
    /// tab's bytes. Recorded by the template runner so an
    /// external-change reload can re-fire the same template
    /// without bothering the user. Cleared when the user
    /// uninstalls / dismisses the template state.
    #[cfg(not(target_arch = "wasm32"))]
    pub last_template_path: Option<PathBuf>,
    /// Per-tab override for the hex view's column count. `None` means
    /// follow the global `AppSettings::hex_columns` default; `Some`
    /// pins this buffer to a specific width regardless of the global
    /// setting. Set via the `Set hex columns (this buffer)` palette
    /// command and not currently persisted across restarts.
    pub hex_columns_override: Option<hxy_core::ColumnCount>,
    /// `Some` when the byte source has a hard write constraint (see
    /// [`ReadOnlyReason`]) that the user cannot override from the
    /// status-bar lock toggle. Forces `editor.edit_mode` to
    /// `Readonly` and rewrites the lock tooltip with the reason.
    pub read_only_reason: Option<ReadOnlyReason>,
    /// Per-tab search bar state. Live as long as the tab; not
    /// persisted across restarts. The bar visibility flag lives on the
    /// state itself rather than a separate boolean so reopening the
    /// bar restores the user's last query.
    pub search: crate::search::SearchState,
    /// User-captured byte snapshots for this tab. Each entry is
    /// mirrored to a sidecar file under `$DATA_DIR/hxy/snapshots/`
    /// so the list survives restarts; small payloads are also
    /// cached in memory for fast comparison. `None` only when the
    /// tab has no stable identity (in-memory scratch buffers
    /// without a persistent path).
    #[cfg(not(target_arch = "wasm32"))]
    pub snapshots: Option<crate::files::snapshot::SnapshotStore>,
    /// Most recent Shannon-entropy result for this tab's bytes.
    /// `None` until the user invokes "Compute entropy" from the
    /// command palette; cleared when an in-flight compute is
    /// kicked off and replaced when it completes.
    #[cfg(not(target_arch = "wasm32"))]
    pub entropy: Option<crate::panels::entropy::EntropyState>,
    /// Worker handle for an in-flight entropy compute. Mutually
    /// exclusive with the populated `entropy` slot in practice
    /// -- starting a recompute clears the old result so the
    /// panel renders the "computing..." placeholder cleanly.
    #[cfg(not(target_arch = "wasm32"))]
    pub entropy_running: Option<crate::panels::entropy::EntropyComputation>,
    /// Per-file visualizer panel state: cached textures, decoded
    /// audio, the user-dismissed flag, the active sub-tab key.
    /// Lazily initialised the first time a visualizer attribute
    /// shows up; outlives template re-runs so dropping then
    /// recreating the same image doesn't re-decode.
    #[cfg(not(target_arch = "wasm32"))]
    pub visualizer_panel: crate::visualizers::VisualizerPanel,
    /// Per-file strings tool state: encoding, min length, the
    /// configured range, the most recent result, and any in-flight
    /// extractor. Default-initialised; the panel renders an empty
    /// placeholder until the user runs the extractor.
    #[cfg(not(target_arch = "wasm32"))]
    pub strings_panel: crate::panels::strings::StringsPanel,
    /// Per-file checksum tool state: enabled algorithm set, the
    /// configured range, the most recent result, and any in-flight
    /// worker. Default selection ticks SHA-256 + BLAKE3.
    #[cfg(not(target_arch = "wasm32"))]
    pub checksums_panel: crate::panels::checksums::ChecksumsPanel,
    /// Identifier for this file's bytes inside the shared byte
    /// cache. Allocated once on construction and reused for every
    /// [`CachedSource`] handle the file or its template runs build.
    /// Callers that remove an [`OpenFile`] from `app.files` must
    /// release the matching cache entries via
    /// [`OpenFile::release_cache`].
    pub source_id: SourceId,
    /// Shared cache reference held alongside [`Self::source_id`] so
    /// reload / save paths can rebuild a [`CachedSource`] under the
    /// same identity.
    pub byte_cache: Arc<ByteCache>,
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

/// Identifier for one template applied to a file. Allocated by the
/// owning [`OpenFile`] so lookups inside [`OpenFile::templates`] don't
/// need to compare paths or ranges. Two instances of the same template
/// run against different ranges get distinct ids.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TemplateInstanceId(u64);

#[cfg(not(target_arch = "wasm32"))]
impl TemplateInstanceId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }
    pub fn get(self) -> u64 {
        self.0
    }
}

/// One completed template applied to a slice of the file. The owned
/// [`TemplateState`] reports node offsets in **file-absolute** coordinates
/// (the runner adjusts them by `range.start()` on the way in), so
/// downstream code -- hex view tinting, breadcrumb tooltips, copy
/// formatting -- doesn't need to know whether the template was run
/// against the whole file or a sub-range.
#[cfg(not(target_arch = "wasm32"))]
pub struct TemplateInstance {
    pub id: TemplateInstanceId,
    /// Path of the template source file. Carried so reload can re-fire
    /// the same template, and so the panel header can show the source.
    pub source_path: PathBuf,
    /// Short name for the tab strip (template's filename or library
    /// display name).
    pub display_name: String,
    /// Byte range of the file the template was bound to. The whole file
    /// for the default "Run template..." flow; a user-picked range for
    /// "Run template at selection..." or for nested templates over
    /// embedded streams (e.g. zlib-decompressed PNG IDAT).
    pub range: ByteRange,
    /// BLAKE3 of the *expanded* template source (post `#include`) at
    /// the moment the run kicked off. Persisted so a restart-time
    /// auto-rerun can detect "the template author edited the file
    /// since last session" -- in which case the node-id-keyed color
    /// overrides are dropped because the indices may no longer align.
    /// `None` for error-only instances where no run happened.
    pub source_fingerprint: Option<[u8; 32]>,
    pub state: TemplateState,
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

/// In-flight run paired with the eventual instance identity. The id is
/// reserved up front so the panel can render a placeholder tab for the
/// running job; on completion the worker's result swaps in under the
/// same id.
#[cfg(not(target_arch = "wasm32"))]
pub struct TemplateRunInstance {
    pub id: TemplateInstanceId,
    pub source_path: PathBuf,
    pub display_name: String,
    pub range: ByteRange,
    /// Hash of the expanded template source as the worker thread saw
    /// it, computed synchronously before the worker is spawned.
    /// Forwarded to the resulting [`TemplateInstance`].
    pub source_fingerprint: Option<[u8; 32]>,
    /// Color overrides to splice into the resulting [`TemplateState`]
    /// once the worker returns -- non-empty only for restart-time
    /// auto-reruns whose persisted fingerprint matched
    /// `source_fingerprint`. Mismatches drop the overrides at runner
    /// kickoff so we never apply them to a node tree they no longer
    /// fit.
    pub pending_overrides: std::collections::HashMap<u32, egui::Color32>,
    pub run: TemplateRun,
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
    /// Currently keyboard-selected row in the panel. Persists across
    /// frames so up/down/left/right have somewhere to operate from
    /// after the user clicked an initial row. Distinct from
    /// `hovered_node` (which follows the pointer) and from the
    /// editor's byte selection (which is what the hex view paints);
    /// click and arrow-key moves both update this AND re-fire the
    /// `Select` side effects so the hex view follows along.
    pub selected_node: Option<TemplateNodeIdx>,
    /// Precomputed (offset, length) spans for every leaf node in
    /// the tree, sorted by offset. Passed to `HexView` so it can
    /// draw field-boundary outlines without walking the tree each
    /// frame.
    pub leaf_boundaries: Vec<(hxy_core::ByteOffset, hxy_core::ByteLen)>,
    /// One tint per entry in `leaf_boundaries`. The hex view uses
    /// these to paint each field's bytes a distinct color when
    /// [`Self::show_colors`] is on. Resolved at construction (and on
    /// every override change) as `node_color_overrides` >
    /// `hxy_color`/`hxy_bg_color` attribute > hue-cycle fallback.
    pub leaf_colors: Vec<egui::Color32>,
    /// Node index for each entry in `leaf_boundaries` / `leaf_colors`.
    /// Lets the panel and override pipeline map "this row" -> "this
    /// field's byte coloring slot" in O(1) via [`Self::leaf_slot_by_node`].
    pub leaf_node_indices: Vec<u32>,
    /// Reverse index: node-tree index -> position in
    /// `leaf_boundaries`. Built once at construction; consulted on
    /// every panel row to decide whether the Color column gets a
    /// swatch (only nodes that actually paint bytes do).
    pub leaf_slot_by_node: std::collections::HashMap<u32, usize>,
    /// User-dialed color overrides, keyed by node-tree index. Take
    /// precedence over template-supplied `hxy_color` and over the
    /// hue-cycle fallback. Persisted across restarts via
    /// [`crate::state::PersistedTemplateInstance::node_color_overrides`].
    pub node_color_overrides: std::collections::HashMap<u32, egui::Color32>,
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
        byte_cache: &Arc<ByteCache>,
    ) -> Self {
        let base: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
        Self::from_source(id, display_name, source_kind, base, byte_cache)
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

    fn cached_view_source(
        cache: &Arc<ByteCache>,
        source_id: SourceId,
        view_key: HexViewKey,
        base: Arc<dyn HexSource>,
    ) -> Arc<dyn HexSource> {
        CachedSource::new(cache.clone(), source_id, Attribution::HexView(view_key), base)
    }

    /// Wrap `base` in a fresh [`CachedSource`] that shares this
    /// file's [`SourceId`] and hex-view attribution. Used by
    /// reload / save paths that need to swap in new bytes while
    /// keeping the cache identity stable.
    pub fn rewrap_for_view(&self, base: Arc<dyn HexSource>) -> Arc<dyn HexSource> {
        Self::cached_view_source(&self.byte_cache, self.source_id, HexViewKey(self.id.get()), base)
    }

    /// Release the cache chunks still attributed to this file.
    /// Called by tab-close / file-removal helpers right before the
    /// [`OpenFile`] is dropped so the cache doesn't keep stale
    /// chunks around under a soon-to-be-reused id.
    pub fn release_cache(&self) {
        self.byte_cache.drop_source(self.source_id);
    }

    /// Construct from any pre-built [`HexSource`]. Wraps it in a
    /// [`hxy_view::HexEditor`] whose patch overlay records future
    /// writes.
    pub fn from_source(
        id: FileId,
        display_name: impl Into<String>,
        source_kind: Option<TabSource>,
        base: Arc<dyn HexSource>,
        byte_cache: &Arc<ByteCache>,
    ) -> Self {
        let source_id = byte_cache.alloc_source_id();
        let view_key = HexViewKey(id.get());
        let cached = Self::cached_view_source(byte_cache, source_id, view_key, base);
        let mut editor = hxy_view::HexEditor::new(cached);
        // Default to mutable whenever we can actually write: pure
        // in-memory buffers always can, filesystem-backed tabs only
        // when the permissions allow. Users who want to explore
        // without touching bytes can still flip the lock.
        let edit_mode = match source_kind.as_ref() {
            Some(TabSource::Filesystem(path)) => Self::default_mode_for_path(path),
            _ => EditMode::Mutable,
        };
        editor.set_edit_mode(edit_mode);
        // Build the snapshot store under the file's stable
        // identity so a previous session's captures are
        // restored. VFS entries / plugin mounts get a key
        // derived from their TabSource string -- different
        // entries inside the same parent get distinct dirs
        // because the entry path is part of the source string.
        #[cfg(not(target_arch = "wasm32"))]
        let snapshots = source_kind.as_ref().map(|src| {
            let key = match src.root_path() {
                Some(p) => p.clone(),
                None => std::path::PathBuf::from(format!("{src:?}")),
            };
            crate::files::snapshot::SnapshotStore::restore(&key)
        });
        Self {
            id,
            display_name: display_name.into(),
            source_kind,
            editor,
            hovered: None,
            detected_handler: None,
            #[cfg(not(target_arch = "wasm32"))]
            templates: Vec::new(),
            #[cfg(not(target_arch = "wasm32"))]
            templates_running: Vec::new(),
            #[cfg(not(target_arch = "wasm32"))]
            active_template: None,
            #[cfg(not(target_arch = "wasm32"))]
            next_template_instance_id: 1,
            #[cfg(not(target_arch = "wasm32"))]
            template_panel_visible: true,
            #[cfg(not(target_arch = "wasm32"))]
            suggested_template: None,
            #[cfg(not(target_arch = "wasm32"))]
            last_template_path: None,
            hex_columns_override: None,
            read_only_reason: None,
            search: crate::search::SearchState::default(),
            #[cfg(not(target_arch = "wasm32"))]
            snapshots,
            #[cfg(not(target_arch = "wasm32"))]
            entropy: None,
            #[cfg(not(target_arch = "wasm32"))]
            entropy_running: None,
            #[cfg(not(target_arch = "wasm32"))]
            visualizer_panel: crate::visualizers::VisualizerPanel::default(),
            #[cfg(not(target_arch = "wasm32"))]
            strings_panel: crate::panels::strings::StringsPanel::default(),
            #[cfg(not(target_arch = "wasm32"))]
            checksums_panel: crate::panels::checksums::ChecksumsPanel::default(),
            source_id,
            byte_cache: byte_cache.clone(),
        }
    }

    /// Convenience: the filesystem path this tab (or any ancestor tab)
    /// ultimately originates from. `None` only for purely in-memory
    /// tabs with no path backing (e.g. placeholder buffers).
    pub fn root_path(&self) -> Option<&PathBuf> {
        self.source_kind.as_ref().and_then(|s| s.root_path())
    }

    /// Allocate a fresh [`TemplateInstanceId`] for a new run on this
    /// tab. Counter is monotonic for the tab's lifetime.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn fresh_template_instance_id(&mut self) -> TemplateInstanceId {
        let id = TemplateInstanceId(self.next_template_instance_id);
        self.next_template_instance_id += 1;
        id
    }

    /// Return the currently-selected template instance, if any. Used by
    /// the hex view (overlay tinting, hover breadcrumbs) and palette
    /// commands that need the "primary" template (e.g. jump to
    /// next/prev field).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn active_template(&self) -> Option<&TemplateInstance> {
        let id = self.active_template?;
        self.templates.iter().find(|t| t.id == id)
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn active_template_mut(&mut self) -> Option<&mut TemplateInstance> {
        let id = self.active_template?;
        self.templates.iter_mut().find(|t| t.id == id)
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn template_instance(&self, id: TemplateInstanceId) -> Option<&TemplateInstance> {
        self.templates.iter().find(|t| t.id == id)
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn template_instance_mut(&mut self, id: TemplateInstanceId) -> Option<&mut TemplateInstance> {
        self.templates.iter_mut().find(|t| t.id == id)
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
    PluginMount { plugin_name: String, token: String, reason: String },
}
