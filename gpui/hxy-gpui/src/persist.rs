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

use crate::panels::FILE_PANEL_NAME;
use crate::panels::STRINGS_PANEL_NAME;
use crate::panels::WELCOME_PANEL_NAME;

/// Bumped when the persisted layout shape changes incompatibly; a
/// mismatch at load time discards the old layout and starts fresh.
pub const LAYOUT_VERSION: usize = 1;

/// Platform data directory, matching the desktop app's scheme so both
/// front-ends live under the same `hxy` folder.
fn storage_dir() -> Option<PathBuf> {
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

/// Drop panels that must not be restored verbatim before handing the
/// state to `DockArea::load`, returning the paths of the file tabs that
/// were dropped so the caller can toast them:
///
/// - `FilePanel` whose backing path no longer reads (deleted/moved):
///   dropped with a warning and its path collected; the tab simply does
///   not come back.
/// - `WelcomePanel`: never restored; the workspace re-adds a fresh one
///   whenever it ends up with zero file tabs, so persisting it would
///   only risk a stale duplicate.
/// - `StringsPanel` whose owning file (recorded path) does not
///   survive pruning as a `FilePanel`: dropped with a `tracing::warn`
///   (no toast -- this is a secondary tab, not a lost file). Computed
///   from a first pass over the surviving `FilePanel` paths so a
///   strings tab's fate never depends on tree walk order relative to
///   its file's tab.
///
/// Side docks are left untouched: the inspector is the only one, and it
/// is rebuilt from the panel registry on restore (or re-added fresh by
/// `Workspace::ensure_inspector_dock`), never pruned here.
pub fn prune_for_restore(state: &mut DockAreaState) -> Vec<PathBuf> {
    let mut pruned = Vec::new();
    let surviving_files = surviving_file_paths(&state.center);
    keep(&mut state.center, &surviving_files, &mut pruned);
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

/// Returns whether `panel` should survive pruning, recursively pruning
/// container children (collecting dropped file paths into `pruned`) and
/// clamping a tab container's active index into the surviving range.
fn keep(panel: &mut PanelState, surviving_files: &HashSet<PathBuf>, pruned: &mut Vec<PathBuf>) -> bool {
    if panel.panel_name == FILE_PANEL_NAME {
        if file_readable(&panel.info) {
            return true;
        }
        if let Some(path) = file_path(&panel.info) {
            pruned.push(path);
        }
        return false;
    }
    if panel.panel_name == WELCOME_PANEL_NAME {
        return false;
    }
    if panel.panel_name == STRINGS_PANEL_NAME {
        return match file_path(&panel.info) {
            Some(path) if surviving_files.contains(&path) => true,
            Some(path) => {
                tracing::warn!(?path, "restore: strings panel's owning file is not open; dropping tab");
                false
            }
            None => {
                tracing::warn!("restore: strings panel has no owning path; dropping tab");
                false
            }
        };
    }

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

/// A restored file tab is kept only if its recorded path still resolves
/// to a regular file that opens for reading -- the same success
/// criterion `FilePanel::restore` uses (`std::fs::read`), so prune and
/// restore never disagree (a directory or a read-denied path is dropped
/// here rather than silently restored as an empty buffer).
/// The recorded `"path"` field of a `FilePanel` or `StringsPanel`
/// leaf's `PanelInfo::Panel` payload, if present -- both panel kinds
/// use the same JSON shape for their owning path.
fn file_path(info: &PanelInfo) -> Option<PathBuf> {
    let PanelInfo::Panel(value) = info else { return None };
    value.get("path").and_then(|p| p.as_str()).map(PathBuf::from)
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

    fn tabs(children: Vec<PanelState>) -> PanelState {
        PanelState { panel_name: "TabPanel".to_string(), children, info: PanelInfo::Tabs { active_index: 0 } }
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
}
