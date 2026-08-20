//! Dock-layout persistence for the GPUI shell.
//!
//! Mirrors the desktop app's platform storage-dir scheme
//! (`crates/hxy/src/settings/persist`) under a gpui-specific filename,
//! writing the [`DockAreaState`] as JSON. The path is injectable so
//! tests can point at a temp dir instead of the real data dir.

use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

use gpui_component::dock::DockAreaState;
use gpui_component::dock::PanelInfo;
use gpui_component::dock::PanelState;

use crate::panels::CHECKSUMS_PANEL_NAME;
use crate::panels::COMPARE_PANEL_NAME;
use crate::panels::ENTROPY_PANEL_NAME;
use crate::panels::FILE_PANEL_NAME;
use crate::panels::GLOBAL_SEARCH_PANEL_NAME;
use crate::panels::STRINGS_PANEL_NAME;
use crate::panels::VISUALIZER_PANEL_NAME;
use crate::panels::WELCOME_PANEL_NAME;
use crate::panels::WORKSPACE_HOST_PANEL_NAME;

/// Bumped when the persisted layout shape changes incompatibly; a
/// mismatch at load time discards the old layout and starts fresh.
pub const LAYOUT_VERSION: usize = 1;

/// Platform data directory, matching the desktop app's scheme so both
/// front-ends live under the same `hxy` folder.
pub(crate) fn storage_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join("Library/Application Support/hxy"))
    }
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var_os("APPDATA")?;
        Some(PathBuf::from(appdata).join("hxy"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
            return Some(PathBuf::from(xdg).join("hxy"));
        }
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join(".local/share/hxy"))
    }
}

/// Default on-disk location for the gpui dock layout, or `None` when no
/// platform data dir resolves (persistence is then disabled).
pub fn layout_path() -> Option<PathBuf> {
    storage_dir().map(|dir| dir.join("gpui-dock-layout.json"))
}

/// Gpui-suffixed snapshot root (`$DATA_DIR/hxy/gpui/snapshots`). Kept
/// separate from the egui app's `$DATA_DIR/hxy/snapshots` so the two
/// front ends never race on the same per-file `index.json`. `None` when
/// no platform data dir resolves. Tests redirect it to a temp dir via
/// [`set_snapshots_base_for_test`] so a capture never touches real data.
pub(crate) fn snapshots_base() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(base) = SNAPSHOTS_BASE_OVERRIDE.with(|cell| cell.borrow().clone()) {
        return Some(base);
    }
    storage_dir().map(|dir| dir.join("gpui").join("snapshots"))
}

#[cfg(test)]
thread_local! {
    static SNAPSHOTS_BASE_OVERRIDE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Redirect [`snapshots_base`] to `path` for the current test thread, so
/// snapshot captures write under a temp dir instead of the real data dir.
#[cfg(test)]
pub(crate) fn set_snapshots_base_for_test(path: PathBuf) {
    SNAPSHOTS_BASE_OVERRIDE.with(|cell| *cell.borrow_mut() = Some(path));
}

/// Read and parse a saved layout. A missing file is a normal cold start
/// (`None`, no warning); a present-but-unparseable file logs and is
/// treated as absent so a schema change never bricks startup.
pub fn load(path: &Path) -> Option<DockAreaState> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            tracing::warn!(?path, %err, "read dock layout failed; using default");
            return None;
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(state) => Some(state),
        Err(err) => {
            tracing::warn!(?path, %err, "parse dock layout failed; using default");
            None
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error("create layout directory {path}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("serialize dock layout")]
    Serialize(#[source] serde_json::Error),
    #[error("write layout file {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub fn save(path: &Path, state: &DockAreaState) -> Result<(), SaveError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|source| SaveError::CreateDir { path: parent.to_path_buf(), source })?;
    }
    let json = serde_json::to_vec_pretty(state).map_err(SaveError::Serialize)?;
    std::fs::write(path, json).map_err(|source| SaveError::Write { path: path.to_path_buf(), source })
}

/// What [`prune_for_restore`] removed, so the caller can toast it. File
/// drops are named individually; stale/foreign tabs (older-layout
/// placeholders with no backing path) are only counted, since there is
/// nothing file-like to name.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PrunedTabs {
    /// File-backed tabs (file panels, workspace hosts) dropped because
    /// their recorded path no longer reads.
    pub dropped_files: Vec<PathBuf>,
    /// Tabs dropped because their panel type is not registered in this
    /// build: stale/foreign leaves and the empty-container quirk. Left
    /// unrestored would rebuild as an uncloseable `InvalidPanel`.
    pub dropped_incompatible: usize,
}

impl PrunedTabs {
    pub fn is_empty(&self) -> bool {
        self.dropped_files.is_empty() && self.dropped_incompatible == 0
    }
}

/// An empty horizontal split, used to replace a center that pruned away
/// entirely (or was a degenerate `Panel(null)` container that would
/// otherwise rebuild as an `InvalidPanel`). `Workspace::reconcile`
/// re-adds the welcome placeholder once the layout loads.
fn empty_center() -> PanelState {
    PanelState {
        panel_name: "StackPanel".to_string(),
        children: Vec::new(),
        info: PanelInfo::Stack { sizes: Vec::new(), axis: 0 },
    }
}

/// Drop panels that must not be restored verbatim before handing the
/// state to `DockArea::load`, returning what was dropped so the caller
/// can toast it:
///
/// - `FilePanel` whose backing path no longer reads (deleted/moved):
///   dropped with a warning and its path collected; the tab simply does
///   not come back.
/// - `WelcomePanel`: never restored; the workspace re-adds a fresh one
///   whenever it ends up with zero file tabs, so persisting it would
///   only risk a stale duplicate.
/// - `StringsPanel` / `EntropyPanel` / `ChecksumsPanel` whose owning
///   file (recorded path) does not survive pruning as a `FilePanel`:
///   dropped with a `tracing::warn` (no toast -- these are secondary
///   tabs, not a lost file). Computed from a first pass over the
///   surviving `FilePanel` paths so no kind's fate depends on tree walk
///   order relative to its file's tab. See [`panel_kind`] for the
///   per-panel-name pruning table these three share.
///
/// Side docks are left untouched: the inspector is the only one, and it
/// is rebuilt from the panel registry on restore (or re-added fresh by
/// `Workspace::ensure_inspector_dock`), never pruned here.
pub fn prune_for_restore(state: &mut DockAreaState) -> PrunedTabs {
    let mut pruned = PrunedTabs::default();
    let surviving_files = surviving_file_paths(&state.center);
    // `keep` never removes the root node itself (the caller owns it), so a
    // center that fully pruned away -- or that arrived as a degenerate
    // `Panel(null)` container -- would still reach `DockArea::load` and
    // rebuild as an `InvalidPanel`. Reset it to an empty tab area instead.
    if !keep(&mut state.center, &surviving_files, &mut pruned) {
        state.center = empty_center();
    }
    pruned
}

/// The set of `FilePanel` paths that will still be present after
/// pruning (i.e. those `file_readable` accepts), gathered up front so
/// `StringsPanel` pruning can check membership regardless of where in
/// the tree its owning file's tab sits.
fn surviving_file_paths(panel: &PanelState) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    collect_surviving_file_paths(panel, &mut out);
    out
}

fn collect_surviving_file_paths(panel: &PanelState, out: &mut HashSet<PathBuf>) {
    if panel.panel_name == FILE_PANEL_NAME {
        if file_readable(&panel.info)
            && let Some(path) = file_path(&panel.info)
        {
            out.insert(path);
        }
        return;
    }
    for child in &panel.children {
        collect_surviving_file_paths(child, out);
    }
}

/// How `keep()` prunes one panel kind. A new per-file secondary panel
/// (strings, entropy, ...) registers one [`PanelKind::OwningPathLeaf`]
/// entry in [`panel_kind`] instead of a duplicated match arm here.
enum PanelKind {
    File,
    /// Never restored; the workspace re-adds a fresh one whenever it
    /// ends up with zero file tabs.
    Welcome,
    /// A per-file leaf tab (strings, entropy, checksums, ...) kept only
    /// when its recorded owning path survived pruning as a `FilePanel`.
    /// `log_label` names the panel kind in the drop warning, e.g.
    /// "strings panel" -> "restore: strings panel's owning file is not
    /// open; dropping tab".
    OwningPathLeaf {
        log_label: &'static str,
    },
    /// A compare tab, kept only when both sides are disk-restorable
    /// (both recorded a path) and both paths still read. A side sourced
    /// from an open file's in-memory buffer records no path, so any such
    /// compare is dropped -- mirrors the egui app dropping open-file-side
    /// compares on restore.
    Compare,
    /// A VFS workspace host tab, kept only when its recorded archive path
    /// (`parent_path`) still reads -- restore re-mounts from it. A missing
    /// or unreadable archive is dropped here. Mountability cannot be
    /// checked at this layer (`prune_for_restore` has no registry access),
    /// so a readable-but-unmountable archive survives pruning and is
    /// handled at restore instead: it falls back to an empty read-only
    /// mount with a warning (see `WorkspaceHostPanel::restore`).
    WorkspaceHost,
    /// A workspace-scoped singleton with no owning-file reference to
    /// validate (the global search tab) -- always restorable verbatim.
    /// Without an explicit arm here, a leaf falls into the generic
    /// container-recursion branch below, which always reports zero
    /// surviving children for a childless leaf and drops it
    /// unconditionally -- the exact bug the entropy panel hit before it
    /// got its own `OwningPathLeaf` arm (see that regression test).
    AlwaysKeep,
}

/// Look up the pruning rule for a panel name, or `None` for a generic
/// container (`TabPanel`/`StackPanel`), which `keep` recurses into.
fn panel_kind(name: &str) -> Option<PanelKind> {
    match name {
        FILE_PANEL_NAME => Some(PanelKind::File),
        WELCOME_PANEL_NAME => Some(PanelKind::Welcome),
        STRINGS_PANEL_NAME => Some(PanelKind::OwningPathLeaf { log_label: "strings panel" }),
        ENTROPY_PANEL_NAME => Some(PanelKind::OwningPathLeaf { log_label: "entropy panel" }),
        CHECKSUMS_PANEL_NAME => Some(PanelKind::OwningPathLeaf { log_label: "checksums panel" }),
        VISUALIZER_PANEL_NAME => Some(PanelKind::OwningPathLeaf { log_label: "visualizer panel" }),
        COMPARE_PANEL_NAME => Some(PanelKind::Compare),
        WORKSPACE_HOST_PANEL_NAME => Some(PanelKind::WorkspaceHost),
        GLOBAL_SEARCH_PANEL_NAME => Some(PanelKind::AlwaysKeep),
        _ => None,
    }
}

/// Returns whether `panel` should survive pruning, recursively pruning
/// container children (collecting dropped file paths into `pruned`) and
/// clamping a tab container's active index into the surviving range.
fn keep(panel: &mut PanelState, surviving_files: &HashSet<PathBuf>, pruned: &mut PrunedTabs) -> bool {
    match panel_kind(&panel.panel_name) {
        Some(PanelKind::File) => {
            if file_readable(&panel.info) {
                return true;
            }
            if let Some(path) = file_path(&panel.info) {
                pruned.dropped_files.push(path);
            }
            false
        }
        Some(PanelKind::Welcome) => false,
        Some(PanelKind::OwningPathLeaf { log_label }) => match file_path(&panel.info) {
            Some(path) if surviving_files.contains(&path) => true,
            Some(path) => {
                tracing::warn!(?path, "restore: {log_label}'s owning file is not open; dropping tab");
                false
            }
            None => {
                tracing::warn!("restore: {log_label} has no owning path; dropping tab");
                false
            }
        },
        Some(PanelKind::Compare) => {
            match (compare_side_path(&panel.info, "a_path"), compare_side_path(&panel.info, "b_path")) {
                (Some(a), Some(b)) if path_is_readable(&a) && path_is_readable(&b) => true,
                _ => {
                    tracing::warn!("restore: compare tab is not disk-restorable; dropping tab");
                    false
                }
            }
        }
        Some(PanelKind::WorkspaceHost) => match workspace_parent_path(&panel.info) {
            Some(path) if path_is_readable(&path) => true,
            Some(path) => {
                tracing::warn!(?path, "restore: workspace archive missing/unreadable; dropping tab");
                // Surface the drop the same way a pruned file tab does
                // (`restore_pruned_texts` names the dropped path).
                pruned.dropped_files.push(path);
                false
            }
            None => {
                tracing::warn!("restore: workspace archive path missing; dropping tab");
                false
            }
        },
        Some(PanelKind::AlwaysKeep) => true,
        // An unregistered name. Split by `info`: a `Panel(_)` leaf is a
        // stale/foreign tab (or the empty-container quirk) and must be
        // dropped so `DockArea::load` never rebuilds it as an uncloseable
        // `InvalidPanel`; anything else is a real container to recurse.
        None => match &panel.info {
            PanelInfo::Panel(value) => {
                // Empty-container quirk: gpui-component 0.5.1 dumps an empty
                // `TabPanel`/`StackPanel` with `info = Panel(null)` (its
                // `dump` only sets `info` inside the child loop). Such a node
                // is indistinguishable from a leaf by name alone. If it still
                // carries children, recover it as a tab container so the
                // surviving tabs are preserved; otherwise drop it. A
                // `Panel(null)`-with-children shape never arises from a real
                // 0.5.1 dump, but is handled defensively here.
                if value.is_null() && !panel.children.is_empty() {
                    panel.children.retain_mut(|child| keep(child, surviving_files, pruned));
                    if panel.children.is_empty() {
                        pruned.dropped_incompatible += 1;
                        return false;
                    }
                    // Re-tag so `to_item` treats it as a tab area, not a
                    // `Panel(_)` leaf bound for `InvalidPanel`.
                    panel.info = PanelInfo::Tabs { active_index: 0 };
                    true
                } else {
                    tracing::warn!(name = %panel.panel_name, "restore: dropping stale/unregistered panel");
                    pruned.dropped_incompatible += 1;
                    false
                }
            }
            _ => {
                panel.children.retain_mut(|child| keep(child, surviving_files, pruned));
                let surviving = panel.children.len();
                // A pruned tab only needs the active index clamped back into range. A
                // pruned split pane leaves `PanelInfo::Stack.sizes` one entry long;
                // `split_with_sizes` indexes sizes defensively (missing -> auto size), so
                // restored pane sizes are at worst slightly off, never a panic. Splits are
                // best-effort here (this shell builds a single center container).
                if let PanelInfo::Tabs { active_index } = &mut panel.info
                    && surviving > 0
                {
                    *active_index = (*active_index).min(surviving - 1);
                }
                surviving != 0
            }
        },
    }
}

/// A restored file tab is kept only if its recorded path still resolves
/// to a regular file that opens for reading -- the same success
/// criterion `FilePanel::restore` uses (`std::fs::read`), so prune and
/// restore never disagree (a directory or a read-denied path is dropped
/// here rather than silently restored as an empty buffer).
/// The recorded `"path"` field of a `FilePanel`, `StringsPanel`, or
/// `EntropyPanel` leaf's `PanelInfo::Panel` payload, if present -- all
/// three panel kinds use the same JSON shape for their owning path.
fn file_path(info: &PanelInfo) -> Option<PathBuf> {
    let PanelInfo::Panel(value) = info else { return None };
    value.get("path").and_then(|p| p.as_str()).map(PathBuf::from)
}

/// A workspace host's recorded archive path (`parent_path`), if present.
fn workspace_parent_path(info: &PanelInfo) -> Option<PathBuf> {
    let PanelInfo::Panel(value) = info else { return None };
    value.get("parent_path").and_then(|p| p.as_str()).map(PathBuf::from)
}

/// One compare side's recorded path (`a_path` / `b_path`), if present.
/// A `null` value (open-file-side, non-restorable) reads as `None`.
fn compare_side_path(info: &PanelInfo, key: &str) -> Option<PathBuf> {
    let PanelInfo::Panel(value) = info else { return None };
    value.get(key).and_then(|p| p.as_str()).map(PathBuf::from)
}

/// Whether a path resolves to a regular file that opens for reading --
/// the same criterion `file_readable` applies, phrased over a path so
/// both compare sides can be checked.
fn path_is_readable(path: &Path) -> bool {
    let is_regular_file = std::fs::metadata(path).map(|meta| meta.is_file()).unwrap_or(false);
    is_regular_file && std::fs::File::open(path).is_ok()
}

fn file_readable(info: &PanelInfo) -> bool {
    let PanelInfo::Panel(value) = info else { return false };
    let Some(path) = value.get("path").and_then(|p| p.as_str()) else { return false };
    let is_regular_file = std::fs::metadata(path).map(|meta| meta.is_file()).unwrap_or(false);
    if !is_regular_file || std::fs::File::open(path).is_err() {
        tracing::warn!(path, "restore: file missing/unreadable; dropping tab");
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use gpui_component::dock::PanelInfo;
    use gpui_component::dock::PanelState;

    use super::*;

    fn file_panel(path: &Path) -> PanelState {
        PanelState {
            panel_name: FILE_PANEL_NAME.to_string(),
            children: Vec::new(),
            info: PanelInfo::panel(serde_json::json!({ "path": path.to_string_lossy() })),
        }
    }

    fn strings_panel(path: Option<&Path>) -> PanelState {
        PanelState {
            panel_name: STRINGS_PANEL_NAME.to_string(),
            children: Vec::new(),
            info: PanelInfo::panel(serde_json::json!({ "path": path.map(|p| p.to_string_lossy()) })),
        }
    }

    fn entropy_panel(path: Option<&Path>) -> PanelState {
        PanelState {
            panel_name: ENTROPY_PANEL_NAME.to_string(),
            children: Vec::new(),
            info: PanelInfo::panel(serde_json::json!({ "path": path.map(|p| p.to_string_lossy()) })),
        }
    }

    fn checksums_panel(path: Option<&Path>) -> PanelState {
        PanelState {
            panel_name: CHECKSUMS_PANEL_NAME.to_string(),
            children: Vec::new(),
            info: PanelInfo::panel(serde_json::json!({ "path": path.map(|p| p.to_string_lossy()) })),
        }
    }

    fn visualizer_panel(path: Option<&Path>) -> PanelState {
        PanelState {
            panel_name: VISUALIZER_PANEL_NAME.to_string(),
            children: Vec::new(),
            info: PanelInfo::panel(serde_json::json!({ "path": path.map(|p| p.to_string_lossy()) })),
        }
    }

    fn global_search_panel() -> PanelState {
        PanelState {
            panel_name: GLOBAL_SEARCH_PANEL_NAME.to_string(),
            children: Vec::new(),
            info: PanelInfo::Panel(serde_json::json!({})),
        }
    }

    fn workspace_host_panel(archive: &Path) -> PanelState {
        PanelState {
            panel_name: WORKSPACE_HOST_PANEL_NAME.to_string(),
            children: Vec::new(),
            info: PanelInfo::panel(serde_json::json!({ "parent_path": archive.to_string_lossy() })),
        }
    }

    /// A workspace host whose archive still reads survives; one whose
    /// archive is gone is pruned and its path is reported (so the caller
    /// can toast it like a dropped file tab).
    #[test]
    fn workspace_host_pruned_when_its_archive_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("present.zip");
        std::fs::write(&present, b"anything").unwrap();
        let missing = dir.path().join("gone.zip");

        let mut state = DockAreaState {
            version: None,
            center: tabs(vec![workspace_host_panel(&present), workspace_host_panel(&missing)]),
            left_dock: None,
            right_dock: None,
            bottom_dock: None,
        };

        let pruned = prune_for_restore(&mut state);

        let names: Vec<&str> = state.center.children.iter().map(|p| p.panel_name.as_str()).collect();
        assert_eq!(names, vec![WORKSPACE_HOST_PANEL_NAME], "only the host with a readable archive survives");
        assert_eq!(pruned.dropped_files, vec![missing], "the missing archive is reported for a warning toast");
    }

    fn tabs(children: Vec<PanelState>) -> PanelState {
        PanelState { panel_name: "TabPanel".to_string(), children, info: PanelInfo::Tabs { active_index: 0 } }
    }

    fn compare_panel(a: Option<&Path>, b: Option<&Path>) -> PanelState {
        PanelState {
            panel_name: COMPARE_PANEL_NAME.to_string(),
            children: Vec::new(),
            info: PanelInfo::panel(serde_json::json!({
                "a_path": a.map(|p| p.to_string_lossy()),
                "b_path": b.map(|p| p.to_string_lossy()),
            })),
        }
    }

    /// A compare tab survives restore only when both sides are disk-
    /// restorable and both paths still read: a null side (open-file
    /// source) or a missing path drops the tab.
    #[test]
    fn compare_tab_kept_only_when_both_sides_are_readable_paths() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.bin");
        let b = dir.path().join("b.bin");
        std::fs::write(&a, b"aaa").unwrap();
        std::fs::write(&b, b"bbb").unwrap();
        let missing = dir.path().join("gone.bin");

        let mut state = DockAreaState {
            version: None,
            center: tabs(vec![
                compare_panel(Some(&a), Some(&b)),       // both readable -> kept
                compare_panel(Some(&a), None),           // open-file B side -> dropped
                compare_panel(Some(&a), Some(&missing)), // unreadable B path -> dropped
            ]),
            left_dock: None,
            right_dock: None,
            bottom_dock: None,
        };

        prune_for_restore(&mut state);

        let names: Vec<&str> = state.center.children.iter().map(|p| p.panel_name.as_str()).collect();
        assert_eq!(names, vec![COMPARE_PANEL_NAME], "only the disk-vs-disk compare survives");
    }

    /// A strings tab whose owning file survives pruning is kept; one
    /// whose file does not (or that never recorded a path) is dropped,
    /// without disturbing the surviving file tab.
    #[test]
    fn strings_panel_kept_only_when_its_owning_file_survives() {
        let dir = tempfile::tempdir().unwrap();
        let kept_path = dir.path().join("kept.bin");
        std::fs::write(&kept_path, b"hello").unwrap();
        let missing_path = dir.path().join("missing.bin");

        let mut state = DockAreaState {
            version: None,
            center: tabs(vec![
                file_panel(&kept_path),
                strings_panel(Some(&kept_path)),
                strings_panel(Some(&missing_path)),
                strings_panel(None),
            ]),
            left_dock: None,
            right_dock: None,
            bottom_dock: None,
        };

        prune_for_restore(&mut state);

        let names: Vec<&str> = state.center.children.iter().map(|p| p.panel_name.as_str()).collect();
        assert_eq!(names, vec![FILE_PANEL_NAME, STRINGS_PANEL_NAME], "only the file and its own strings tab survive");
        assert_eq!(file_path(&state.center.children[1].info), Some(kept_path));
    }

    /// Same rule as the strings panel, for entropy: a tab whose owning
    /// file survives pruning is kept; one whose file does not (or that
    /// never recorded a path) is dropped. Regression test for the
    /// entropy panel review finding: `keep()` originally had no
    /// `ENTROPY_PANEL_NAME` branch, so every entropy leaf fell into the
    /// generic container-recursion arm, which always reports zero
    /// surviving children for a leaf and drops it unconditionally.
    #[test]
    fn entropy_panel_kept_only_when_its_owning_file_survives() {
        let dir = tempfile::tempdir().unwrap();
        let kept_path = dir.path().join("kept.bin");
        std::fs::write(&kept_path, b"hello").unwrap();
        let missing_path = dir.path().join("missing.bin");

        let mut state = DockAreaState {
            version: None,
            center: tabs(vec![
                file_panel(&kept_path),
                entropy_panel(Some(&kept_path)),
                entropy_panel(Some(&missing_path)),
                entropy_panel(None),
            ]),
            left_dock: None,
            right_dock: None,
            bottom_dock: None,
        };

        prune_for_restore(&mut state);

        let names: Vec<&str> = state.center.children.iter().map(|p| p.panel_name.as_str()).collect();
        assert_eq!(names, vec![FILE_PANEL_NAME, ENTROPY_PANEL_NAME], "only the file and its own entropy tab survive");
        assert_eq!(file_path(&state.center.children[1].info), Some(kept_path));
    }

    /// Same rule again, for checksums: the fourth panel kind registered
    /// in `panel_kind` rather than a fourth duplicated match arm in
    /// `keep()`.
    #[test]
    fn checksums_panel_kept_only_when_its_owning_file_survives() {
        let dir = tempfile::tempdir().unwrap();
        let kept_path = dir.path().join("kept.bin");
        std::fs::write(&kept_path, b"hello").unwrap();
        let missing_path = dir.path().join("missing.bin");

        let mut state = DockAreaState {
            version: None,
            center: tabs(vec![
                file_panel(&kept_path),
                checksums_panel(Some(&kept_path)),
                checksums_panel(Some(&missing_path)),
                checksums_panel(None),
            ]),
            left_dock: None,
            right_dock: None,
            bottom_dock: None,
        };

        prune_for_restore(&mut state);

        let names: Vec<&str> = state.center.children.iter().map(|p| p.panel_name.as_str()).collect();
        assert_eq!(
            names,
            vec![FILE_PANEL_NAME, CHECKSUMS_PANEL_NAME],
            "only the file and its own checksums tab survive"
        );
        assert_eq!(file_path(&state.center.children[1].info), Some(kept_path));
    }

    /// Same rule again, for the visualizer panel: kept only while its
    /// owning file's tab survives, dropped otherwise (including when it
    /// never recorded a path).
    #[test]
    fn visualizer_panel_kept_only_when_its_owning_file_survives() {
        let dir = tempfile::tempdir().unwrap();
        let kept_path = dir.path().join("kept.bin");
        std::fs::write(&kept_path, b"hello").unwrap();
        let missing_path = dir.path().join("missing.bin");

        let mut state = DockAreaState {
            version: None,
            center: tabs(vec![
                file_panel(&kept_path),
                visualizer_panel(Some(&kept_path)),
                visualizer_panel(Some(&missing_path)),
                visualizer_panel(None),
            ]),
            left_dock: None,
            right_dock: None,
            bottom_dock: None,
        };

        prune_for_restore(&mut state);

        let names: Vec<&str> = state.center.children.iter().map(|p| p.panel_name.as_str()).collect();
        assert_eq!(
            names,
            vec![FILE_PANEL_NAME, VISUALIZER_PANEL_NAME],
            "only the file and its own visualizer tab survive"
        );
        assert_eq!(file_path(&state.center.children[1].info), Some(kept_path));
    }

    /// A childless global-search leaf survives pruning unconditionally --
    /// it has no owning-file path to validate. Regression test: without
    /// its own `PanelKind::AlwaysKeep` arm, this leaf falls into the
    /// generic container-recursion branch, which reports zero surviving
    /// children for any childless leaf and drops it (the same bug class
    /// `entropy_panel_kept_only_when_its_owning_file_survives` guards).
    #[test]
    fn global_search_panel_always_survives_pruning() {
        let mut state = DockAreaState {
            version: None,
            center: tabs(vec![global_search_panel()]),
            left_dock: None,
            right_dock: None,
            bottom_dock: None,
        };

        prune_for_restore(&mut state);

        let names: Vec<&str> = state.center.children.iter().map(|p| p.panel_name.as_str()).collect();
        assert_eq!(names, vec![GLOBAL_SEARCH_PANEL_NAME]);
    }

    /// A node that `DockArea::load` would rebuild as an `InvalidPanel`:
    /// a `Panel(_)` leaf whose name is not a registered leaf kind (an
    /// unknown/foreign name, or a container name carrying the empty-
    /// container `Panel(null)` quirk).
    fn any_invalid_panel_leaf(panel: &PanelState) -> bool {
        let here = matches!(panel.info, PanelInfo::Panel(_)) && panel_kind(&panel.panel_name).is_none();
        here || panel.children.iter().any(any_invalid_panel_leaf)
    }

    /// The user's stale-layout symptom, as JSON: an older build saved a
    /// center holding an empty `TabPanel` serialized as a LEAF
    /// (gpui-component 0.5.1 dumps an empty container with
    /// `info = {"panel": null}`), beside a still-readable file tab.
    /// Restored verbatim, `DockArea::load` rebuilds the leaf as an
    /// uncloseable `InvalidPanel` titled "The `TabPanel` panel type is not
    /// registered". Pruning must drop the placeholder and keep the file.
    #[test]
    fn stale_container_as_leaf_is_dropped_and_file_tab_survives() {
        let dir = tempfile::tempdir().unwrap();
        let kept = dir.path().join("kept.bin");
        std::fs::write(&kept, b"hello").unwrap();

        let json = format!(
            r#"{{
              "version": 1,
              "center": {{
                "panel_name": "StackPanel",
                "children": [
                  {{ "panel_name": "TabPanel", "children": [], "info": {{ "panel": null }} }},
                  {{ "panel_name": "FilePanel", "children": [], "info": {{ "panel": {{ "path": {path:?} }} }} }}
                ],
                "info": {{ "stack": {{ "sizes": [], "axis": 0 }} }}
              }},
              "left_dock": null,
              "right_dock": null,
              "bottom_dock": null
            }}"#,
            path = kept.to_string_lossy(),
        );
        let mut state: DockAreaState = serde_json::from_str(&json).expect("fixture parses");

        let pruned = prune_for_restore(&mut state);

        assert!(!any_invalid_panel_leaf(&state.center), "no node left that would rebuild as InvalidPanel");
        let names: Vec<&str> = state.center.children.iter().map(|p| p.panel_name.as_str()).collect();
        assert_eq!(names, vec![FILE_PANEL_NAME], "the readable file tab survives");
        assert_eq!(pruned.dropped_incompatible, 1, "the placeholder is counted as a dropped incompatible tab");
        assert!(pruned.dropped_files.is_empty(), "no file was dropped");
    }

    /// A leaf whose panel name is not registered at all (a panel type
    /// removed since the layout was saved) is dropped and counted, never
    /// rebuilt as an `InvalidPanel`.
    #[test]
    fn unknown_leaf_name_is_dropped_with_incompatible_count() {
        let mut state = DockAreaState {
            version: Some(LAYOUT_VERSION),
            center: tabs(vec![PanelState {
                panel_name: "GhostPanel".to_string(),
                children: Vec::new(),
                info: PanelInfo::panel(serde_json::json!({ "stale": true })),
            }]),
            left_dock: None,
            right_dock: None,
            bottom_dock: None,
        };

        let pruned = prune_for_restore(&mut state);

        assert!(!any_invalid_panel_leaf(&state.center));
        assert_eq!(pruned.dropped_incompatible, 1);
        assert!(pruned.dropped_files.is_empty());
    }

    /// The whole center arriving as a degenerate `Panel(null)` container
    /// leaf: `keep` never removes the root itself, so `prune_for_restore`
    /// must reset it to a real (empty) container rather than let
    /// `DockArea::load` rebuild it as an `InvalidPanel`.
    #[test]
    fn degenerate_root_center_is_reset_not_left_as_invalid_panel() {
        let mut state = DockAreaState {
            version: Some(LAYOUT_VERSION),
            center: PanelState {
                panel_name: "TabPanel".to_string(),
                children: Vec::new(),
                info: PanelInfo::Panel(serde_json::Value::Null),
            },
            left_dock: None,
            right_dock: None,
            bottom_dock: None,
        };

        let pruned = prune_for_restore(&mut state);

        assert!(!any_invalid_panel_leaf(&state.center), "root reset to a real container");
        assert!(matches!(state.center.info, PanelInfo::Stack { .. }));
        assert_eq!(pruned.dropped_incompatible, 1);
    }

    /// The defensive branch of fix (b): a `Panel(null)` container that
    /// still carries children is recovered as a tab area so the surviving
    /// tabs are preserved (this shape never arises from a real 0.5.1 dump,
    /// but must not lose data if it ever does).
    #[test]
    fn null_info_container_with_children_is_recovered_as_tabs() {
        let dir = tempfile::tempdir().unwrap();
        let kept = dir.path().join("kept.bin");
        std::fs::write(&kept, b"hello").unwrap();

        let mut state = DockAreaState {
            version: Some(LAYOUT_VERSION),
            center: PanelState {
                panel_name: "TabPanel".to_string(),
                children: vec![file_panel(&kept)],
                info: PanelInfo::Panel(serde_json::Value::Null),
            },
            left_dock: None,
            right_dock: None,
            bottom_dock: None,
        };

        let pruned = prune_for_restore(&mut state);

        assert!(matches!(state.center.info, PanelInfo::Tabs { .. }), "recovered to a tab container");
        assert!(!any_invalid_panel_leaf(&state.center));
        let names: Vec<&str> = state.center.children.iter().map(|p| p.panel_name.as_str()).collect();
        assert_eq!(names, vec![FILE_PANEL_NAME]);
        assert_eq!(pruned.dropped_incompatible, 0, "nothing was dropped; the file was preserved");
    }
}
