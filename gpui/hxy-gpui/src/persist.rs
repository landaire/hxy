//! Dock-layout persistence for the GPUI shell.
//!
//! Mirrors the desktop app's platform storage-dir scheme
//! (`crates/hxy/src/settings/persist`) under a gpui-specific filename,
//! writing the [`DockAreaState`] as JSON. The path is injectable so
//! tests can point at a temp dir instead of the real data dir.

use std::path::Path;
use std::path::PathBuf;

use gpui_component::dock::DockAreaState;
use gpui_component::dock::PanelInfo;
use gpui_component::dock::PanelState;

use crate::panels::FILE_PANEL_NAME;
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
/// state to `DockArea::load`:
///
/// - `FilePanel` whose backing path no longer reads (deleted/moved):
///   dropped with a warning; the tab simply does not come back (toast
///   integration is Task 6).
/// - `WelcomePanel`: never restored; the workspace re-adds a fresh one
///   whenever it ends up with zero file tabs, so persisting it would
///   only risk a stale duplicate.
///
/// Side docks are left untouched: this shell never creates them, and
/// `DockState`'s fields are private, so there is nothing to walk.
pub fn prune_for_restore(state: &mut DockAreaState) {
    keep(&mut state.center);
}

/// Returns whether `panel` should survive pruning, recursively pruning
/// container children and clamping a tab container's active index into
/// the surviving range.
fn keep(panel: &mut PanelState) -> bool {
    if panel.panel_name == FILE_PANEL_NAME {
        return file_readable(&panel.info);
    }
    if panel.panel_name == WELCOME_PANEL_NAME {
        return false;
    }

    panel.children.retain_mut(keep);
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
