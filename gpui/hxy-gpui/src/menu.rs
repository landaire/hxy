//! Native menu bar (macOS `NSMenu` via `gpui::App::set_menus`; other
//! platforms only get the `Menu`/`MenuItem` data, no bar -- see
//! `docs/superpowers/plans/2026-07-25-m2-gpui-component-notes.md` #5)
//! and the shared action set / keybindings backing it.
//!
//! Every item here dispatches a gpui [`Action`](gpui::Action) that is
//! also bound in [`init_keybindings`], so the menu and the keyboard
//! shortcut always agree. macOS shows the shortcut next to the item
//! automatically by looking up the action's registered keybinding
//! (`platform/mac/platform.rs::create_menu_item`), so item labels here
//! carry no accelerator text.
//!
//! gpui 0.2.2 CAN grey out a menu item dynamically (the platform calls
//! back into `App::is_action_available`, which walks the currently
//! focused element's dispatch path for a registered `on_action`
//! listener). But `Workspace` binds every handler below once, on its
//! own root `div`, which sits on every focus path regardless of editor
//! state -- so `is_action_available` always reports "available" and
//! every item here is permanently enabled. Genuine semantic
//! enable/disable (grey Undo when the undo stack is empty) would need
//! per-state conditional handler registration, out of scope for M2:
//! every handler below no-ops gracefully when there is nothing to do
//! (no active file, empty undo stack, no selection, ...).
//!
//! Parity source: `crates/hxy/src/menu.rs:28-58` (egui's `MenuAction`).
//! Ships a subset -- no New/Paste (not implemented in the gpui port
//! yet). Save / Save As / Reopen Closed Tab landed with the M3 save +
//! dirty-close work; Settings (open-or-focus, Cmd+Comma like egui's
//! Toggle Settings) landed with M4e; Plugins and Console (both
//! open-or-focus) landed with M4.

use gpui::App;
use gpui::Menu;
use gpui::MenuItem;
use gpui::actions;

use crate::workspace::OpenChecksums;
use crate::workspace::OpenConsole;
use crate::workspace::OpenEntropy;
use crate::workspace::OpenFile;
use crate::workspace::OpenPlugins;
use crate::workspace::OpenSettings;
use crate::workspace::OpenSnapshots;
use crate::workspace::OpenStrings;
use crate::workspace::ReopenClosedTab;
use crate::workspace::Save;
use crate::workspace::SaveAs;
use crate::workspace::TakeSnapshot;
use crate::workspace::ToggleGlobalSearch;
use crate::workspace::ToggleInspector;
use crate::workspace::ToggleVim;

actions!(hxy_gpui_menu, [ShowAbout, Quit, CloseTab, Undo, Redo, ToggleEditMode, CopyBytes, CopyHex]);

/// Register the menu-only keybindings (the File/View actions they
/// share -- `OpenFile`, `ToggleVim`, `ToggleInspector` -- are already
/// bound by [`crate::workspace::init_keybindings`]). Call once at
/// startup, alongside that function.
pub fn init_keybindings(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("cmd-w", CloseTab, None),
        gpui::KeyBinding::new("cmd-z", Undo, None),
        gpui::KeyBinding::new("cmd-shift-z", Redo, None),
        gpui::KeyBinding::new("cmd-e", ToggleEditMode, None),
        gpui::KeyBinding::new("cmd-c", CopyBytes, None),
        gpui::KeyBinding::new("cmd-shift-c", CopyHex, None),
        gpui::KeyBinding::new("cmd-q", Quit, None),
    ]);
}

/// Register the App-menu actions that have no window-specific
/// behavior. `Quit` needs no `Window`, so it is a true global listener
/// (fires regardless of which element is focused); `ShowAbout` opens a
/// dialog and so is handled as a `Workspace` action instead (see
/// `Workspace::on_show_about`).
pub fn init_global_actions(cx: &mut App) {
    cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
}

/// Build the native menu bar: App (About/Quit), File (Open, Close
/// Tab), Edit (Undo, Redo, Toggle Edit Mode, Copy Bytes, Copy Hex),
/// View (Toggle Inspector, Toggle Vim). All titles route through
/// `hxy_i18n`, reusing the same keys the egui menu and command palette
/// already use for the shared items.
pub fn build_menus() -> Vec<Menu> {
    vec![
        Menu {
            name: hxy_i18n::t("app-name").into(),
            items: vec![
                MenuItem::action(hxy_i18n::t("menu-help-about"), ShowAbout),
                MenuItem::separator(),
                MenuItem::action(hxy_i18n::t("menu-file-quit"), Quit),
            ],
            disabled: false,
        },
        Menu {
            name: hxy_i18n::t("menu-file").into(),
            items: vec![
                MenuItem::action(hxy_i18n::t("menu-file-open"), OpenFile),
                MenuItem::separator(),
                MenuItem::action(hxy_i18n::t("menu-file-save"), Save),
                MenuItem::action(hxy_i18n::t("menu-file-save-as"), SaveAs),
                MenuItem::separator(),
                MenuItem::action(hxy_i18n::t("menu-file-reopen-closed"), ReopenClosedTab),
                MenuItem::action(hxy_i18n::t("gpui-palette-close-tab"), CloseTab),
            ],
            disabled: false,
        },
        Menu {
            name: hxy_i18n::t("menu-edit").into(),
            items: vec![
                MenuItem::action(hxy_i18n::t("menu-edit-undo"), Undo),
                MenuItem::action(hxy_i18n::t("menu-edit-redo"), Redo),
                MenuItem::separator(),
                MenuItem::action(hxy_i18n::t("gpui-menu-toggle-edit-mode"), ToggleEditMode),
                MenuItem::separator(),
                MenuItem::action(hxy_i18n::t("menu-edit-copy-bytes"), CopyBytes),
                MenuItem::action(hxy_i18n::t("menu-edit-copy-hex"), CopyHex),
            ],
            disabled: false,
        },
        Menu {
            name: hxy_i18n::t("menu-view").into(),
            items: vec![
                MenuItem::action(hxy_i18n::t("gpui-palette-toggle-inspector"), ToggleInspector),
                MenuItem::action(hxy_i18n::t("gpui-palette-toggle-global-search"), ToggleGlobalSearch),
                MenuItem::action(hxy_i18n::t("palette-toggle-vim"), ToggleVim),
                MenuItem::separator(),
                MenuItem::action(hxy_i18n::t("palette-strings-whole-file"), OpenStrings),
                MenuItem::action(hxy_i18n::t("palette-compute-entropy"), OpenEntropy),
                MenuItem::action(hxy_i18n::t("palette-checksums-whole-file"), OpenChecksums),
                MenuItem::separator(),
                MenuItem::action(hxy_i18n::t("gpui-menu-take-snapshot"), TakeSnapshot),
                MenuItem::action(hxy_i18n::t("gpui-menu-snapshots"), OpenSnapshots),
                MenuItem::separator(),
                MenuItem::action(hxy_i18n::t("tab-settings"), OpenSettings),
                MenuItem::action(hxy_i18n::t("tab-plugins"), OpenPlugins),
                MenuItem::action(hxy_i18n::t("tab-console"), OpenConsole),
            ],
            disabled: false,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The native `NSMenu` itself only exists on a real macOS run loop
    /// and can't be inspected headlessly (no `get_menus()` round-trip
    /// in a `TestAppContext`), but the `Vec<Menu>` this builds is plain
    /// data: pin its shape so a future edit can't silently drop a menu
    /// or rename a title without the parity list above being updated.
    #[test]
    fn menu_shape_matches_the_m2_inventory() {
        let menus = build_menus();
        let names: Vec<String> = menus.iter().map(|m| m.name.to_string()).collect();
        assert_eq!(
            names,
            vec![hxy_i18n::t("app-name"), hxy_i18n::t("menu-file"), hxy_i18n::t("menu-edit"), hxy_i18n::t("menu-view")]
        );

        let item_count = |ix: usize| menus[ix].items.len();
        assert_eq!(item_count(0), 3, "App menu: About, separator, Quit");
        assert_eq!(item_count(1), 7, "File menu: Open, sep, Save, Save As, sep, Reopen Closed Tab, Close Tab");
        assert_eq!(item_count(2), 7, "Edit menu: Undo, Redo, sep, Toggle Edit Mode, sep, Copy Bytes, Copy Hex");
        assert_eq!(
            item_count(3),
            14,
            "View menu: Toggle Inspector, Toggle Global Search, Toggle Vim, sep, Strings, Entropy, Checksums, sep, Take Snapshot, Snapshots, sep, Settings, Plugins, Console"
        );
    }
}
