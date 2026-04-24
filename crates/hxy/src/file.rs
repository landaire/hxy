//! Per-tab open-file state.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;

use hxy_core::ByteOffset;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_core::PatchedSource;
use hxy_core::Selection;
use hxy_vfs::MountedVfs;
use hxy_vfs::TabSource;
use hxy_vfs::VfsHandler;
use suture::Patch;
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

/// Whether writes through [`OpenFile::request_write`] are accepted.
/// New tabs default to [`EditMode::Readonly`]; the user toggles to
/// [`EditMode::Mutable`] explicitly. Mirrors 010 Editor's "edit mode"
/// gate: the readonly default avoids accidental edits while exploring.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EditMode {
    #[default]
    Readonly,
    Mutable,
}

#[derive(Debug, Error)]
pub enum WriteError {
    #[error("file is read-only; switch to mutable edit mode first")]
    Readonly,
    #[error("write at {offset} extends past source length {source_len}")]
    OutOfBounds { offset: u64, len: u64, source_len: u64 },
    #[error("write rejected: {0}")]
    Rejected(String),
}

/// Single reversible edit: the byte range `[offset, offset+len)`, the
/// bytes that were there before the edit, and the bytes that replaced
/// them. Length-preserving, so `old_bytes.len() == new_bytes.len()`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct EditEntry {
    pub offset: u64,
    pub old_bytes: Vec<u8>,
    pub new_bytes: Vec<u8>,
}

impl EditEntry {
    fn end(&self) -> u64 {
        self.offset + self.new_bytes.len() as u64
    }
}

/// Cap on how many separate undo entries we retain per tab. Old entries
/// fall off the bottom when the cap is exceeded -- a hex editor doesn't
/// benefit from bottomless history and a million single-byte edits
/// would balloon memory.
const UNDO_HISTORY_CAP: usize = 1000;

/// Idle interval after which a new write stops coalescing into the
/// previous undo entry. Matches the "pause to think" cadence so a
/// short run of typing stays one undo unit but a deliberate second
/// edit made after a beat reads as a separate logical change.
const EDIT_COALESCE_IDLE: std::time::Duration = std::time::Duration::from_millis(800);

pub struct OpenFile {
    pub id: FileId,
    pub display_name: String,
    /// Persistent identity of the tab's byte source. `None` for
    /// temporary in-memory buffers that shouldn't survive a restart.
    pub source_kind: Option<TabSource>,
    /// Patched view of the underlying bytes. Readers (hex view,
    /// inspector, template runner, exporters) all consume this and
    /// transparently see edits from `patch`. The same `Arc` is held
    /// by the `PatchedSource` (so the patch is shared) and by
    /// readers (so they can clone freely for worker threads).
    pub source: Arc<dyn HexSource>,
    /// Shared handle into the [`PatchedSource`]'s patch. Mutated by
    /// [`OpenFile::request_write`] and friends; read by the dirty
    /// indicator and the save path.
    pub patch: Arc<RwLock<Patch>>,
    pub edit_mode: EditMode,
    /// Two-press hex-digit input state. `true` means the next typed
    /// digit overwrites the high nibble of the byte at the cursor;
    /// `false` means the low nibble. Reset to `true` on any cursor
    /// move, mode toggle, or new tab.
    pub edit_high_nibble: bool,
    /// Cursor offset observed at the end of the previous frame.
    /// Compared each frame to detect cursor moves and reset the
    /// nibble pointer; otherwise typing after an arrow-key move
    /// would land on the wrong nibble.
    pub last_cursor_offset: Option<u64>,
    /// Which of the row's two panes currently owns keyboard input.
    /// Updated when the user clicks into the hex or ASCII pane;
    /// drives how `dispatch_hex_edit_keys` interprets typed
    /// characters (hex nibble vs literal ASCII byte). Defaults to
    /// [`hxy_view::Pane::Hex`].
    pub active_pane: hxy_view::Pane,
    /// Undo stack: most recent edit at the end. Consecutive writes
    /// that overlap or abut each other coalesce into a single entry
    /// unless a boundary has been pushed (via navigation, mode
    /// toggle, save, revert, etc.).
    pub undo_stack: Vec<EditEntry>,
    /// Redo stack: entries popped by `undo()` land here and get
    /// reapplied by `redo()`. Cleared whenever a fresh write
    /// diverges from the undone branch.
    pub redo_stack: Vec<EditEntry>,
    /// `true` when the next `request_write` must start a fresh undo
    /// entry rather than coalesce into the previous one. Set by
    /// `push_history_boundary`, `undo`, `redo`, and `revert` (which
    /// also clears both stacks).
    pub history_break: bool,
    /// Monotonic instant of the most recent successful write. Used
    /// by the coalescing rule to push a fresh undo entry whenever
    /// enough idle time has passed between keystrokes (see
    /// [`EDIT_COALESCE_IDLE`]). Monotonic (not wall-clock) so NTP
    /// adjustments can't accidentally collapse or split entries.
    /// `None` until the first write lands.
    pub last_edit_at: Option<std::time::Instant>,
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
    /// [`PatchedSource`] so future writes record into the per-tab
    /// patch.
    pub fn from_source(
        id: FileId,
        display_name: impl Into<String>,
        source_kind: Option<TabSource>,
        base: Arc<dyn HexSource>,
    ) -> Self {
        let patched = PatchedSource::new(base);
        let patch = patched.patch();
        // Default to mutable whenever we can actually write: pure
        // in-memory buffers always can, filesystem-backed tabs only
        // when the permissions allow. Users who want to explore
        // without touching bytes can still flip the lock.
        let edit_mode = match source_kind.as_ref() {
            Some(TabSource::Filesystem(path)) => Self::default_mode_for_path(path),
            _ => EditMode::Mutable,
        };
        Self {
            id,
            display_name: display_name.into(),
            source_kind,
            source: Arc::new(patched),
            patch,
            edit_mode,
            edit_high_nibble: true,
            last_cursor_offset: None,
            active_pane: hxy_view::Pane::Hex,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            history_break: false,
            last_edit_at: None,
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

    /// `true` when the per-tab patch carries any pending edits.
    pub fn is_dirty(&self) -> bool {
        !self.patch.read().expect("patch lock poisoned").is_empty()
    }

    /// Record a length-preserving write through the edit-mode gate.
    /// Errors when the tab is in [`EditMode::Readonly`] or the write
    /// would extend past the current source length. Also records
    /// the edit in the undo stack, coalescing with the most recent
    /// entry when the ranges touch and no boundary has been pushed.
    pub fn request_write(&mut self, offset: u64, bytes: Vec<u8>) -> Result<(), WriteError> {
        if self.edit_mode != EditMode::Mutable {
            return Err(WriteError::Readonly);
        }
        let source_len = self.source.len().get();
        let end = offset + bytes.len() as u64;
        if end > source_len {
            return Err(WriteError::OutOfBounds { offset, len: bytes.len() as u64, source_len });
        }
        let mut old_bytes = Vec::with_capacity(bytes.len());
        for i in 0..bytes.len() {
            old_bytes.push(self.read_byte_at(offset + i as u64)?);
        }
        self.patch
            .write()
            .expect("patch lock poisoned")
            .write(offset, bytes.clone())
            .map_err(|e| WriteError::Rejected(e.to_string()))?;
        self.redo_stack.clear();
        let now = std::time::Instant::now();
        let idle_break = self.last_edit_at.is_some_and(|last| now.duration_since(last) >= EDIT_COALESCE_IDLE);
        let entry = EditEntry { offset, old_bytes, new_bytes: bytes };
        if !self.history_break
            && !idle_break
            && let Some(last) = self.undo_stack.last_mut()
            && ranges_touch(last.offset, last.end(), entry.offset, entry.end())
        {
            merge_entry(last, &entry);
        } else {
            self.undo_stack.push(entry);
            if self.undo_stack.len() > UNDO_HISTORY_CAP {
                self.undo_stack.remove(0);
            }
        }
        self.history_break = false;
        self.last_edit_at = Some(now);
        Ok(())
    }

    /// Drop all pending edits and clear both history stacks.
    pub fn revert(&mut self) {
        *self.patch.write().expect("patch lock poisoned") = Patch::new();
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.history_break = true;
    }

    /// Force the next `request_write` to start a new undo entry
    /// rather than coalesce with the previous one. Called on cursor
    /// navigation, edit-mode toggle, save, and anywhere else an
    /// interactive boundary should end a coalescing run.
    pub fn push_history_boundary(&mut self) {
        self.history_break = true;
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    /// Revert the most recent undo entry. Moves it to the redo stack,
    /// rebuilds the patch from the remaining undo entries, and sets
    /// a history boundary so the next write starts a fresh entry.
    /// Returns the reverted entry so callers can realign the cursor.
    pub fn undo(&mut self) -> Option<EditEntry> {
        if self.edit_mode != EditMode::Mutable {
            return None;
        }
        let entry = self.undo_stack.pop()?;
        self.rebuild_patch_from_stack();
        self.redo_stack.push(entry.clone());
        self.history_break = true;
        self.edit_high_nibble = true;
        Some(entry)
    }

    /// Re-apply the top entry on the redo stack. Returns the entry so
    /// callers can realign the cursor. Sets a history boundary so a
    /// subsequent write starts fresh rather than coalescing into the
    /// just-redone entry.
    pub fn redo(&mut self) -> Option<EditEntry> {
        if self.edit_mode != EditMode::Mutable {
            return None;
        }
        let entry = self.redo_stack.pop()?;
        if let Err(e) = self
            .patch
            .write()
            .expect("patch lock poisoned")
            .write(entry.offset, entry.new_bytes.clone())
        {
            tracing::warn!(error = %e, "redo write rejected; restoring redo stack");
            self.redo_stack.push(entry);
            return None;
        }
        self.undo_stack.push(entry.clone());
        self.history_break = true;
        self.edit_high_nibble = true;
        Some(entry)
    }

    /// Rebuild the tab's patch from the current undo stack. After any
    /// undo we discard the patch and replay every surviving entry so
    /// fully undoing back to zero leaves the patch empty (and
    /// `is_dirty()` false). Preserves any [`suture::metadata::SourceMetadata`]
    /// already attached.
    fn rebuild_patch_from_stack(&self) {
        let mut patch = self.patch.write().expect("patch lock poisoned");
        let metadata = patch.metadata().cloned();
        *patch = match metadata {
            Some(m) => Patch::with_metadata(m),
            None => Patch::new(),
        };
        for entry in &self.undo_stack {
            if let Err(e) = patch.write(entry.offset, entry.new_bytes.clone()) {
                tracing::warn!(error = %e, "rebuild_patch_from_stack: entry rejected");
            }
        }
    }

    /// Apply one hex-digit keystroke at the current cursor offset.
    /// Returns `true` if a write was actually issued (so the caller
    /// can advance the cursor on a low-nibble press). Silent no-op
    /// when the tab is read-only or the cursor isn't set.
    pub fn type_hex_digit(&mut self, nibble: u8) -> Result<bool, WriteError> {
        if self.edit_mode != EditMode::Mutable {
            return Err(WriteError::Readonly);
        }
        let nibble = nibble & 0xF;
        let Some(selection) = self.selection else { return Ok(false) };
        let offset = selection.cursor.get();
        let source_len = self.source.len().get();
        if offset >= source_len {
            return Ok(false);
        }
        let current = self.read_byte_at(offset)?;
        let new_byte = if self.edit_high_nibble {
            (nibble << 4) | (current & 0x0F)
        } else {
            (current & 0xF0) | nibble
        };
        self.request_write(offset, vec![new_byte])?;
        let advanced = !self.edit_high_nibble;
        self.edit_high_nibble = !self.edit_high_nibble;
        Ok(advanced)
    }

    /// Reset the two-press nibble cursor to "expecting high nibble".
    /// Call when the cursor moves, when the file enters edit mode,
    /// or when the user explicitly cancels a half-typed byte.
    pub fn reset_edit_nibble(&mut self) {
        self.edit_high_nibble = true;
    }

    /// Write a single byte at the current cursor, used when the
    /// active pane is ASCII. Returns `true` if a write was actually
    /// issued so the caller can advance the cursor. Unlike the hex
    /// path this is a one-shot per keystroke: there's no high/low
    /// nibble state to track.
    pub fn type_ascii_byte(&mut self, byte: u8) -> Result<bool, WriteError> {
        if self.edit_mode != EditMode::Mutable {
            return Err(WriteError::Readonly);
        }
        let Some(selection) = self.selection else { return Ok(false) };
        let offset = selection.cursor.get();
        if offset >= self.source.len().get() {
            return Ok(false);
        }
        self.request_write(offset, vec![byte])?;
        Ok(true)
    }

    fn read_byte_at(&self, offset: u64) -> Result<u8, WriteError> {
        use hxy_core::ByteOffset;
        use hxy_core::ByteRange;
        let range = ByteRange::new(ByteOffset::new(offset), ByteOffset::new(offset + 1))
            .map_err(|e| WriteError::Rejected(format!("invalid range: {e}")))?;
        self.source.read(range).map(|b| b[0]).map_err(|e| WriteError::Rejected(format!("read: {e}")))
    }

    /// Snapshot the current patch as a sorted list of `(start, end)`
    /// output-space byte ranges. Used by the hex view to tint
    /// modified bytes; binary-searched per row, so O(log N).
    ///
    /// With the current length-preserving `request_write` API,
    /// output offsets equal source offsets and `end - start` equals
    /// the splice's `new_bytes.len()`. The mapping will need
    /// rewriting if we expose insert / delete to the editor.
    pub fn modified_ranges(&self) -> Vec<(u64, u64)> {
        self.patch
            .read()
            .expect("patch lock poisoned")
            .ops()
            .iter()
            .map(|op| (op.offset, op.offset + op.new_bytes.len() as u64))
            .collect()
    }
}

/// True when two half-open `[start, end)` byte ranges overlap or are
/// exactly adjacent. Adjacent runs (e.g. typing consecutive bytes)
/// should coalesce into one undo entry.
fn ranges_touch(a_start: u64, a_end: u64, b_start: u64, b_end: u64) -> bool {
    a_start <= b_end && b_start <= a_end
}

/// Merge `next` into `dst` in place, extending `dst`'s range to
/// cover the union and painting `next`'s new bytes on top of any
/// overlapping tail. `dst.old_bytes` only grows where the range
/// extends past what was already tracked -- original bytes inside
/// an already-edited span were captured by the first write and must
/// not be clobbered by the later value we just wrote.
fn merge_entry(dst: &mut EditEntry, next: &EditEntry) {
    let start = dst.offset.min(next.offset);
    let end = dst.end().max(next.end());
    let len = (end - start) as usize;
    let mut new_bytes = vec![0u8; len];
    let mut old_bytes = vec![0u8; len];
    let dst_off = (dst.offset - start) as usize;
    new_bytes[dst_off..dst_off + dst.new_bytes.len()].copy_from_slice(&dst.new_bytes);
    old_bytes[dst_off..dst_off + dst.old_bytes.len()].copy_from_slice(&dst.old_bytes);
    let next_off = (next.offset - start) as usize;
    new_bytes[next_off..next_off + next.new_bytes.len()].copy_from_slice(&next.new_bytes);
    if next.offset < dst.offset {
        let prefix_len = (dst.offset - next.offset) as usize;
        old_bytes[0..prefix_len].copy_from_slice(&next.old_bytes[..prefix_len]);
    }
    if next.end() > dst.end() {
        let overlap = (dst.end().saturating_sub(next.offset)) as usize;
        let tail = &next.old_bytes[overlap..];
        let tail_off = (dst.end() - start) as usize;
        old_bytes[tail_off..tail_off + tail.len()].copy_from_slice(tail);
    }
    dst.offset = start;
    dst.new_bytes = new_bytes;
    dst.old_bytes = old_bytes;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> OpenFile {
        OpenFile::from_bytes(FileId::new(1), "t", None, vec![0x00, 0x11, 0x22, 0x33, 0x44, 0x55])
    }

    #[test]
    fn consecutive_writes_coalesce_into_one_undo_entry() {
        let mut f = sample();
        f.request_write(1, vec![0xAA]).unwrap();
        f.request_write(1, vec![0xAB]).unwrap();
        f.request_write(2, vec![0xCC]).unwrap();
        assert_eq!(f.undo_stack.len(), 1);
        let e = &f.undo_stack[0];
        assert_eq!(e.offset, 1);
        assert_eq!(e.new_bytes, vec![0xAB, 0xCC]);
        assert_eq!(e.old_bytes, vec![0x11, 0x22]);
    }

    #[test]
    fn history_boundary_starts_a_new_entry() {
        let mut f = sample();
        f.request_write(1, vec![0xAA]).unwrap();
        f.push_history_boundary();
        f.request_write(2, vec![0xBB]).unwrap();
        assert_eq!(f.undo_stack.len(), 2);
    }

    #[test]
    fn idle_gap_starts_a_new_entry() {
        let mut f = sample();
        f.request_write(1, vec![0xAA]).unwrap();
        // Back-date last_edit_at so the next write looks like it came
        // after the idle cutoff. 2s stays safely above the 800ms
        // threshold even on slow CI runners.
        let backdated = f.last_edit_at.unwrap() - std::time::Duration::from_secs(2);
        f.last_edit_at = Some(backdated);
        f.request_write(2, vec![0xBB]).unwrap();
        assert_eq!(f.undo_stack.len(), 2);
    }

    #[test]
    fn fast_writes_still_coalesce() {
        // Without an idle gap or explicit boundary, adjacent writes
        // merge into a single undo entry.
        let mut f = sample();
        f.request_write(1, vec![0xAA]).unwrap();
        f.request_write(2, vec![0xBB]).unwrap();
        assert_eq!(f.undo_stack.len(), 1);
    }

    #[test]
    fn undo_returns_to_clean_state() {
        let mut f = sample();
        f.request_write(1, vec![0xAA]).unwrap();
        f.request_write(2, vec![0xBB]).unwrap();
        assert!(f.is_dirty());
        assert!(f.undo().is_some());
        assert!(!f.is_dirty());
        assert_eq!(f.undo_stack.len(), 0);
        assert_eq!(f.redo_stack.len(), 1);
    }

    #[test]
    fn redo_reapplies_the_edit() {
        let mut f = sample();
        f.request_write(1, vec![0xAA]).unwrap();
        f.undo().unwrap();
        assert!(!f.is_dirty());
        f.redo().unwrap();
        assert!(f.is_dirty());
        assert_eq!(f.undo_stack.len(), 1);
        assert_eq!(f.redo_stack.len(), 0);
    }

    #[test]
    fn fresh_write_after_undo_clears_redo() {
        let mut f = sample();
        f.request_write(1, vec![0xAA]).unwrap();
        f.undo().unwrap();
        assert_eq!(f.redo_stack.len(), 1);
        f.push_history_boundary();
        f.request_write(3, vec![0xDD]).unwrap();
        assert_eq!(f.redo_stack.len(), 0);
    }

    #[test]
    fn revert_clears_both_stacks() {
        let mut f = sample();
        f.request_write(1, vec![0xAA]).unwrap();
        f.undo().unwrap();
        f.revert();
        assert_eq!(f.undo_stack.len(), 0);
        assert_eq!(f.redo_stack.len(), 0);
    }

    #[test]
    fn ascii_byte_write_advances_through_coalescing() {
        let mut f = sample();
        f.selection = Some(hxy_core::Selection::caret(hxy_core::ByteOffset::new(1)));
        assert!(f.type_ascii_byte(b'H').unwrap());
        // Simulate cursor advance the dispatcher does.
        let sel = f.selection.as_mut().unwrap();
        sel.cursor = hxy_core::ByteOffset::new(2);
        sel.anchor = sel.cursor;
        assert!(f.type_ascii_byte(b'i').unwrap());
        assert_eq!(f.undo_stack.len(), 1);
        assert_eq!(f.undo_stack[0].new_bytes, vec![b'H', b'i']);
    }

    #[test]
    fn ascii_byte_write_rejects_readonly() {
        let mut f = sample();
        f.edit_mode = EditMode::Readonly;
        f.selection = Some(hxy_core::Selection::caret(hxy_core::ByteOffset::new(0)));
        assert!(matches!(f.type_ascii_byte(b'A'), Err(WriteError::Readonly)));
    }

    #[test]
    fn readonly_blocks_undo_and_redo() {
        let mut f = sample();
        f.request_write(1, vec![0xAA]).unwrap();
        f.edit_mode = EditMode::Readonly;
        assert!(f.undo().is_none());
        f.edit_mode = EditMode::Mutable;
        f.undo().unwrap();
        f.edit_mode = EditMode::Readonly;
        assert!(f.redo().is_none());
    }

    #[test]
    fn undo_cap_drops_oldest() {
        let mut f = sample();
        for _ in 0..(UNDO_HISTORY_CAP + 5) {
            f.push_history_boundary();
            f.request_write(0, vec![0x01]).unwrap();
        }
        assert_eq!(f.undo_stack.len(), UNDO_HISTORY_CAP);
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
