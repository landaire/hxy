//! Dock panels hosted in the workbench and their layout-restore
//! registration.
//!
//! Three panel kinds live here: [`FilePanel`] (a hex view over one open
//! file), [`WelcomePanel`] (the empty-workspace placeholder), and
//! [`InspectorPanel`] (the data-decoder right dock). All three are
//! registered with gpui-component's `PanelRegistry` under stable names
//! so a persisted layout can rebuild them.

use std::sync::Arc;

use gpui::App;
use gpui::AppContext;
use gpui_component::dock::PanelHandle;
use gpui_component::dock::register_panel;

pub mod checksums;
pub mod compare;
pub mod console_view;
pub mod entropy;
mod file;
pub mod global_search;
pub mod inspector;
pub mod plugins_view;
mod search_bar;
pub mod settings_view;
pub mod strings;
pub mod template_view;
pub mod vfs_tree;
pub mod visualizer;
mod welcome;
pub mod workspace_host;

pub use checksums::CHECKSUMS_PANEL_NAME;
pub use checksums::ChecksumsPanel;
pub use compare::COMPARE_PANEL_NAME;
pub use compare::ComparePanel;
pub use console_view::CONSOLE_PANEL_NAME;
pub use console_view::ConsolePanel;
pub use entropy::ENTROPY_PANEL_NAME;
pub use entropy::EntropyPanel;
pub use file::FILE_PANEL_NAME;
pub use file::FilePanel;
pub use file::OpenVisualizerRequested;
pub use file::TemplateConsoleLog;
pub use global_search::GLOBAL_SEARCH_PANEL_NAME;
pub use global_search::GlobalSearchPanel;
pub use inspector::INSPECTOR_PANEL_NAME;
pub use inspector::InspectorPanel;
pub use plugins_view::FetchImhexPatternsRequested;
pub use plugins_view::PLUGINS_PANEL_NAME;
pub use plugins_view::PluginsPanel;
pub use settings_view::SETTINGS_PANEL_NAME;
pub use settings_view::SettingsPanel;
pub use strings::STRINGS_PANEL_NAME;
pub use strings::StringsPanel;
pub use visualizer::VISUALIZER_PANEL_NAME;
pub use visualizer::VisualizerPanel;
pub use welcome::OpenRecentRequested;
pub use welcome::WELCOME_PANEL_NAME;
pub use welcome::WelcomePanel;
pub use workspace_host::PluginMountIdentity;
pub use workspace_host::WORKSPACE_HOST_PANEL_NAME;
pub use workspace_host::WorkspaceHostPanel;

/// Register every panel name so `DockArea::load` can rebuild a saved
/// layout. Must run once at startup, before any layout is loaded.
pub fn register(cx: &mut App) {
    register_panel(cx, FILE_PANEL_NAME, |ctx, window, cx| {
        let info = ctx.info();
        Arc::new(PanelHandle::new(cx.new(|cx| FilePanel::restore(info, window, cx))))
    });
    register_panel(cx, WELCOME_PANEL_NAME, |_ctx, _window, cx| {
        Arc::new(PanelHandle::new(cx.new(WelcomePanel::new)))
    });
    register_panel(cx, INSPECTOR_PANEL_NAME, |ctx, _window, cx| {
        let info = ctx.info();
        Arc::new(PanelHandle::new(cx.new(|cx| InspectorPanel::restore(info, cx))))
    });
    register_panel(cx, STRINGS_PANEL_NAME, |ctx, window, cx| {
        let info = ctx.info();
        Arc::new(PanelHandle::new(cx.new(|cx| StringsPanel::restore(info, window, cx))))
    });
    register_panel(cx, ENTROPY_PANEL_NAME, |ctx, window, cx| {
        let info = ctx.info();
        Arc::new(PanelHandle::new(cx.new(|cx| EntropyPanel::restore(info, window, cx))))
    });
    register_panel(cx, CHECKSUMS_PANEL_NAME, |ctx, window, cx| {
        let info = ctx.info();
        Arc::new(PanelHandle::new(cx.new(|cx| ChecksumsPanel::restore(info, window, cx))))
    });
    register_panel(cx, COMPARE_PANEL_NAME, |ctx, window, cx| {
        let info = ctx.info();
        Arc::new(PanelHandle::new(cx.new(|cx| ComparePanel::restore(info, window, cx))))
    });
    register_panel(cx, VISUALIZER_PANEL_NAME, |ctx, window, cx| {
        let info = ctx.info();
        Arc::new(PanelHandle::new(cx.new(|cx| VisualizerPanel::restore(info, window, cx))))
    });
    // Settings restores fresh: the panel reads the live settings
    // global and carries no per-instance state worth persisting.
    register_panel(cx, SETTINGS_PANEL_NAME, |_ctx, window, cx| {
        Arc::new(PanelHandle::new(cx.new(|cx| SettingsPanel::new(window, cx))))
    });
    // Plugins restores fresh: the panel reads the live handler registry
    // and settings globals, carrying no per-instance state.
    register_panel(cx, PLUGINS_PANEL_NAME, |_ctx, window, cx| {
        Arc::new(PanelHandle::new(cx.new(|cx| PluginsPanel::new(window, cx))))
    });
    // Console restores fresh: it renders the live console-log global and
    // carries no per-instance state worth persisting.
    register_panel(cx, CONSOLE_PANEL_NAME, |_ctx, _window, cx| {
        Arc::new(PanelHandle::new(cx.new(ConsolePanel::new)))
    });
    // Global search restores as a fresh, empty panel -- egui likewise
    // persists only the `SearchResults` tab marker, not the query
    // (`crates/hxy/src/tabs/persisted_dock.rs`'s `PersistedTab::SearchResults`).
    register_panel(cx, GLOBAL_SEARCH_PANEL_NAME, |_ctx, window, cx| {
        Arc::new(PanelHandle::new(cx.new(|cx| GlobalSearchPanel::new(window, cx))))
    });
    workspace_host::register(cx);
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use gpui_component::dock::DockArea;
    use gpui_component::dock::PanelBuildContext;
    use gpui_component::dock::PanelInfo;
    use gpui_component::dock::PanelRegistry;
    use gpui_component::dock::PanelState;

    use super::*;

    /// Every name `register` claims to support must build a real panel,
    /// not `PanelRegistry`'s silent `InvalidPanel` fallback for an
    /// unregistered name. This matters more than it looks: `InvalidPanel`
    /// swallows the failure quietly (it even echoes the original
    /// `PanelState` back out of `dump`), so a dropped/typo'd
    /// `register_panel` call would otherwise pass any test that only
    /// checks the persisted JSON, not the live rebuilt panel's identity.
    #[gpui::test]
    fn every_registered_name_builds_a_real_panel(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            register(cx);
        });
        let window = cx.add_window(|window, cx| DockArea::new("test", None, window, cx));
        window
            .update(cx, |_dock, window, cx| {
                let weak = cx.entity().downgrade();
                for name in [
                    FILE_PANEL_NAME,
                    CONSOLE_PANEL_NAME,
                    WELCOME_PANEL_NAME,
                    INSPECTOR_PANEL_NAME,
                    STRINGS_PANEL_NAME,
                    ENTROPY_PANEL_NAME,
                    CHECKSUMS_PANEL_NAME,
                    COMPARE_PANEL_NAME,
                    VISUALIZER_PANEL_NAME,
                    GLOBAL_SEARCH_PANEL_NAME,
                    SETTINGS_PANEL_NAME,
                    PLUGINS_PANEL_NAME,
                    WORKSPACE_HOST_PANEL_NAME,
                ] {
                    let state = PanelState {
                        panel_name: name.to_string(),
                        children: Vec::new(),
                        info: PanelInfo::panel(serde_json::json!({})),
                    };
                    let ctx = PanelBuildContext::new(weak.clone(), &state, &state.info);
                    let view = PanelRegistry::build_panel(name, ctx, window, cx)
                        .unwrap_or_else(|| panic!("{name} must build a real panel, not an unregistered name"));
                    assert_eq!(view.panel_name(cx), name, "{name} must build a real panel, not InvalidPanel");
                }
            })
            .unwrap();
    }
}
