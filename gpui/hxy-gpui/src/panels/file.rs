//! [`FilePanel`]: one open file rendered through a [`HexPane`], with an
//! in-file search/replace bar ([`SearchBar`]) that slots in below it,
//! wrapped as a dock [`Panel`] so it can live in a tab and round-trip
//! through layout persistence.

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
use gpui::InteractiveElement;
use gpui::IntoElement;
use gpui::ParentElement;
use gpui::Render;
use gpui::SharedString;
use gpui::Styled;
use gpui::Window;
use gpui::div;
use gpui::prelude::FluentBuilder;
use gpui_component::dock::Panel;
use gpui_component::dock::PanelEvent;
use gpui_component::dock::PanelInfo;
use gpui_component::dock::PanelState;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_vfs::VfsHandler;
use hxy_view_gpui::HexPane;

use super::search_bar::SearchBar;
use crate::workspace::CloseSearch;
use crate::workspace::ToggleSearch;

/// Stable identifier for layout (de)serialization; must never change.
pub const FILE_PANEL_NAME: &str = "FilePanel";

pub struct FilePanel {
    pane: Entity<HexPane>,
    path: Option<PathBuf>,
    /// Tab label for a pathless buffer -- set for a VFS entry (its leaf
    /// name) so the tab reads sensibly instead of "Untitled".
    title_override: Option<String>,
    /// VFS handler that claimed this file's header, if any, enabling the
    /// "Browse VFS" command. `None` for a plain file or a VFS entry.
    detected_handler: Option<Arc<dyn VfsHandler>>,
    search: Entity<SearchBar>,
}

impl FilePanel {
    pub fn new(source: Arc<dyn HexSource>, path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let pane = cx.new(|cx| HexPane::new(source, cx));
        let search = cx.new(|cx| SearchBar::new(pane.clone(), window, cx));
        Self { pane, path, title_override: None, detected_handler: None, search }
    }

    /// A file panel over a VFS entry: no on-disk path, but a stable tab
    /// title (the entry's leaf name).
    pub fn new_vfs_entry(
        source: Arc<dyn HexSource>,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut panel = Self::new(source, None, window, cx);
        panel.title_override = Some(title);
        panel
    }

    /// Record the VFS handler that matched this file's header (enables
    /// the "Browse VFS" command for this tab).
    pub fn set_detected_handler(&mut self, handler: Option<Arc<dyn VfsHandler>>) {
        self.detected_handler = handler;
    }

    /// The VFS handler matching this file, if one was detected.
    pub fn detected_handler(&self) -> Option<Arc<dyn VfsHandler>> {
        self.detected_handler.clone()
    }

    /// Rebuild a panel from persisted [`PanelInfo`]. The path is
    /// re-read from disk; callers prune unreadable paths before restore
    /// (see `persist::prune_for_restore`), so a read failure here is
    /// only a defensive fallback to an empty buffer. VFS-handler
    /// detection is re-run on the re-read bytes exactly as a fresh open
    /// does, so "Browse VFS" stays enabled for a restored zip tab
    /// (mirrors the egui app re-detecting on session restore).
    pub fn restore(info: &PanelInfo, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path = path_from_info(info);
        let bytes: Vec<u8> = match &path {
            Some(path) => match std::fs::read(path) {
                Ok(bytes) => bytes,
                Err(err) => {
                    tracing::warn!(?path, %err, "restore: re-read failed; empty buffer");
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        let handler = crate::panels::workspace_host::detect_handler(cx, &bytes[..bytes.len().min(4096)]);
        let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
        let mut panel = Self::new(source, path, window, cx);
        panel.detected_handler = handler;
        panel
    }

    pub fn pane(&self) -> &Entity<HexPane> {
        &self.pane
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The tab label: the VFS entry title if set, else the file leaf name,
    /// else the untitled placeholder.
    fn tab_label(&self) -> String {
        match &self.title_override {
            Some(title) => title.clone(),
            None => tab_title(self.path.as_deref()),
        }
    }

    /// `cmd-f`: open the bar (focusing the query field) or, if already
    /// open, close it and hand focus back to the grid.
    fn on_toggle_search(&mut self, _: &ToggleSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |bar, cx| bar.toggle(window, cx));
    }

    /// `escape`, scoped to the search bar's own key context so it only
    /// fires while a search input has focus (see [`SearchBar::render`]'s
    /// `key_context`); the input's own escape handling propagates here
    /// when it doesn't consume the key itself (not `clean_on_escape`).
    fn on_close_search(&mut self, _: &CloseSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |bar, cx| bar.close(window, cx));
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
        SharedString::from(self.tab_label())
    }

    /// Same text as `title`, just via the `&self` (non-rendering) path
    /// `Panel::tab_name` provides -- defaults to `None`, which would
    /// otherwise leave every file leaf's pane-picker row unlabeled.
    fn tab_name(&self, _cx: &App) -> Option<SharedString> {
        Some(SharedString::from(self.tab_label()))
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.search.read(cx).is_open();
        div()
            .size_full()
            .flex()
            .flex_col()
            .on_action(cx.listener(Self::on_toggle_search))
            .on_action(cx.listener(Self::on_close_search))
            .child(div().flex_1().min_h_0().child(self.pane.clone()))
            .when(open, |root| root.child(self.search.clone()))
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::*;

    fn setup(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::workspace::init_keybindings(cx);
        });
    }

    fn source() -> Arc<dyn HexSource> {
        Arc::new(MemorySource::new(vec![0u8; 16]))
    }

    /// Builds a `FilePanel` inside a real `gpui_component::Root` window
    /// (like the production shell does): `InputState`'s focus tracking
    /// -- and thus the search bar's query field -- needs the Root layer
    /// present, not just dialogs.
    fn build(cx: &mut TestAppContext) -> (Entity<FilePanel>, &mut gpui::VisualTestContext) {
        let window = cx.add_window(|window, cx| {
            let panel = cx.new(|cx| FilePanel::new(source(), None, window, cx));
            gpui_component::Root::new(panel, window, cx)
        });
        let root = window.root(cx).unwrap();
        let panel = root.read_with(cx, |root, _| root.view().clone().downcast::<FilePanel>().unwrap());
        let vcx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        vcx.run_until_parked();
        (panel, vcx)
    }

    /// `cmd-f` opens the bar and focuses the query field; `escape`
    /// closes it again and hands focus back to the grid -- the exact
    /// focus flow the search bar's UX depends on.
    #[gpui::test]
    fn cmd_f_opens_and_escape_closes_and_refocuses_grid(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx);

        let grid_handle = panel.read_with(cx, |panel, cx| panel.pane.read(cx).focus_handle(cx));
        cx.update(|window, _cx| window.focus(&grid_handle));
        cx.run_until_parked();
        assert_eq!(cx.update(|window, cx| window.focused(cx)), Some(grid_handle.clone()), "grid starts focused");

        cx.simulate_keystrokes("cmd-f");
        let (is_open, query_handle) =
            panel.read_with(cx, |panel, cx| (panel.search.read(cx).is_open(), panel.search.read(cx).focus_handle(cx)));
        assert!(is_open, "cmd-f opens the search bar");
        assert_eq!(cx.update(|window, cx| window.focused(cx)), Some(query_handle), "cmd-f focuses the query input");

        cx.simulate_keystrokes("escape");
        let is_open_after = panel.read_with(cx, |panel, cx| panel.search.read(cx).is_open());
        assert!(!is_open_after, "escape closes the search bar");
        assert_eq!(cx.update(|window, cx| window.focused(cx)), Some(grid_handle), "escape refocuses the grid");
    }

    /// A restored zip-backed tab re-runs VFS-handler detection on the
    /// re-read bytes, so "Browse VFS" stays enabled across a relaunch
    /// (regression: `restore` previously left `detected_handler` `None`).
    #[gpui::test]
    fn restore_re_detects_the_vfs_handler(cx: &mut TestAppContext) {
        // `crate::panels::register` installs the VFS registry global that
        // detection reads; the plain `setup` above does not.
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::panels::register(cx);
        });
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("fixture.zip");
        std::fs::write(&archive, super::super::vfs_tree::test_support::fixture_zip_bytes()).unwrap();

        let info = PanelInfo::panel(serde_json::json!({ "path": archive.to_string_lossy() }));
        let window = cx.add_window(|window, cx| {
            let panel = cx.new(|cx| FilePanel::restore(&info, window, cx));
            gpui_component::Root::new(panel, window, cx)
        });
        let panel = window.root(cx).unwrap().read_with(cx, |r, _| r.view().clone().downcast::<FilePanel>().unwrap());
        let vcx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        vcx.run_until_parked();

        panel.read_with(vcx, |panel, _| {
            assert!(panel.detected_handler().is_some(), "restored zip tab re-detects its handler");
        });
    }
}
