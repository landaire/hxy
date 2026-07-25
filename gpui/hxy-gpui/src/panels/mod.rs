//! Dock panels hosted in the workbench and their layout-restore
//! registration.
//!
//! Two panel kinds live here: [`FilePanel`] (a hex view over one open
//! file) and [`WelcomePanel`] (the empty-workspace placeholder). Both
//! are registered with gpui-component's [`PanelRegistry`] under stable
//! names so a persisted layout can rebuild them.

use gpui::App;
use gpui::AppContext;
use gpui_component::dock::PanelView;
use gpui_component::dock::register_panel;

mod file;
mod welcome;

pub use file::FILE_PANEL_NAME;
pub use file::FilePanel;
pub use welcome::WELCOME_PANEL_NAME;
pub use welcome::WelcomePanel;

/// Register both panel names so `DockArea::load` can rebuild a saved
/// layout. Must run once at startup, before any layout is loaded.
pub fn register(cx: &mut App) {
    register_panel(cx, FILE_PANEL_NAME, |_dock, _state, info, _window, cx| {
        Box::new(cx.new(|cx| FilePanel::restore(info, cx)))
    });
    register_panel(cx, WELCOME_PANEL_NAME, |_dock, _state, _info, _window, cx| {
        Box::new(cx.new(WelcomePanel::new)) as Box<dyn PanelView>
    });
}
