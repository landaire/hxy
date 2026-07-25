//! [`FilePanel`]: one open file rendered through a [`HexPane`], wrapped
//! as a dock [`Panel`] so it can live in a tab and round-trip through
//! layout persistence.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::App;
use gpui::AppContext;
use gpui::Context;
use gpui::Entity;
use gpui::EventEmitter;
use gpui::FocusHandle;
use gpui::Focusable;
use gpui::IntoElement;
use gpui::ParentElement;
use gpui::Render;
use gpui::SharedString;
use gpui::Styled;
use gpui::Window;
use gpui::div;
use gpui_component::dock::Panel;
use gpui_component::dock::PanelEvent;
use gpui_component::dock::PanelInfo;
use gpui_component::dock::PanelState;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_view_gpui::HexPane;

/// Stable identifier for layout (de)serialization; must never change.
pub const FILE_PANEL_NAME: &str = "FilePanel";

pub struct FilePanel {
    pane: Entity<HexPane>,
    path: Option<PathBuf>,
}

impl FilePanel {
    pub fn new(source: Arc<dyn HexSource>, path: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let pane = cx.new(|cx| HexPane::new(source, cx));
        Self { pane, path }
    }

    /// Rebuild a panel from persisted [`PanelInfo`]. The path is
    /// re-read from disk; callers prune unreadable paths before restore
    /// (see `persist::prune_for_restore`), so a read failure here is
    /// only a defensive fallback to an empty buffer.
    pub fn restore(info: &PanelInfo, cx: &mut Context<Self>) -> Self {
        let path = path_from_info(info);
        let source: Arc<dyn HexSource> = match &path {
            Some(path) => match std::fs::read(path) {
                Ok(bytes) => Arc::new(MemorySource::new(bytes)),
                Err(err) => {
                    tracing::warn!(?path, %err, "restore: re-read failed; empty buffer");
                    Arc::new(MemorySource::new(Vec::new()))
                }
            },
            None => Arc::new(MemorySource::new(Vec::new())),
        };
        Self::new(source, path, cx)
    }

    pub fn pane(&self) -> &Entity<HexPane> {
        &self.pane
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

/// Extract the stored file path from a `FilePanel` payload, if present.
fn path_from_info(info: &PanelInfo) -> Option<PathBuf> {
    let PanelInfo::Panel(value) = info else { return None };
    value.get("path").and_then(|p| p.as_str()).map(PathBuf::from)
}

/// The panel base name shown on its tab: the file's leaf name, falling
/// back to the full path when it has none, or the untitled placeholder
/// when the panel has no path.
fn tab_title(path: Option<&Path>) -> String {
    match path {
        Some(path) => path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
        None => hxy_i18n::t("gpui-file-untitled"),
    }
}

impl Panel for FilePanel {
    fn panel_name(&self) -> &'static str {
        FILE_PANEL_NAME
    }

    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        SharedString::from(tab_title(self.path.as_deref()))
    }

    /// Persist the backing path so the tab can be re-opened next launch.
    fn dump(&self, _cx: &App) -> PanelState {
        let mut state = PanelState::new(self);
        let path = self.path.as_ref().map(|p| p.to_string_lossy().into_owned());
        state.info = PanelInfo::panel(serde_json::json!({ "path": path }));
        state
    }
}

impl Focusable for FilePanel {
    /// Delegate focus to the inner [`HexPane`] so the dock focusing the
    /// active panel lands directly on the grid that owns the key
    /// handlers -- typing reaches the editor with no extra click.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.pane.read(cx).focus_handle(cx)
    }
}

impl EventEmitter<PanelEvent> for FilePanel {}

impl Render for FilePanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.pane.clone())
    }
}
