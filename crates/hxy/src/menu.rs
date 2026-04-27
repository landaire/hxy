//! Native macOS menu bar via `muda`. Builds an `NSMenu` mirroring the
//! in-window egui menu, dispatches clicks back to the app as
//! [`MenuAction`]s, and syncs per-frame enable state (Copy items grey
//! out when no file is open, etc.).
//!
//! Only compiled on macOS. On other platforms the app falls back to
//! the egui top-bar menu in `app.rs`.

use std::collections::HashMap;

use muda::AboutMetadata;
use muda::Menu;
use muda::MenuEvent;
use muda::MenuItem;
use muda::PredefinedMenuItem;
use muda::Submenu;
use muda::accelerator::Accelerator;
use muda::accelerator::Code;
use muda::accelerator::Modifiers;

use crate::APP_NAME;
use crate::files::copy::CopyKind;

/// Actions a menu item can dispatch. Produced by [`MenuState::drain_actions`]
/// each frame; the app matches on the variant and invokes the same
/// handlers the egui menu would.
#[derive(Clone, Copy, Debug)]
pub enum MenuAction {
    /// Create a fresh anonymous (scratch) tab named `Untitled N`.
    NewFile,
    OpenFile,
    /// Save the active tab in place. Falls back to Save As when the
    /// tab has no path backing it.
    Save,
    SaveAs,
    /// Close the focused tab. File tabs with unsaved edits trigger
    /// the "Save before closing?" modal; others close immediately.
    CloseTab,
    /// Flip the active tab between read-only and mutable.
    ToggleEditMode,
    Undo,
    Redo,
    CopyBytes,
    CopyHex,
    CopyAs(CopyKind),
    /// Paste clipboard text as raw UTF-8 bytes at the cursor.
    Paste,
    /// Paste clipboard text interpreted as hex bytes at the cursor.
    PasteAsHex,
    ToggleConsole,
    ToggleInspector,
    TogglePlugins,
}

/// Owns the muda [`Menu`] (dropping it tears down the `NSMenu`) and
/// the id-to-action map used to translate incoming events. Held by
/// [`crate::HxyApp`] on macOS.
pub struct MenuState {
    actions: HashMap<String, MenuAction>,
    bytes_items: Vec<MenuItem>,
    scalar_items: Vec<MenuItem>,
    /// Save and Save As; greyed out unless the active tab is dirty
    /// or has a path to write to.
    save_items: Vec<MenuItem>,
    /// Toggle Edit Mode entries; greyed out when no file is active.
    edit_mode_items: Vec<MenuItem>,
    /// Undo entry; greyed out when the active tab has no undo history.
    undo_items: Vec<MenuItem>,
    /// Redo entry; greyed out when the active tab has no redo history.
    redo_items: Vec<MenuItem>,
    /// Paste / Paste as hex; greyed out when no writable tab is active.
    paste_items: Vec<MenuItem>,
    _menu: Menu,
}

impl MenuState {
    /// Build the menu bar and install it as the NSApp main menu. Must
    /// be called on the main thread *after* `NSApplication` has been
    /// initialised -- call from `HxyApp::new`, which runs on eframe's
    /// `CreationContext` (window already created).
    pub fn install() -> Self {
        disable_automatic_window_tabbing();
        let menu = Menu::new();
        let mut actions: HashMap<String, MenuAction> = HashMap::new();

        let app_menu = Submenu::new(APP_NAME, true);
        menu.append(&app_menu).expect("append app menu");
        let about_metadata = AboutMetadata {
            name: Some(APP_NAME.to_string()),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
            ..Default::default()
        };
        app_menu
            .append_items(&[
                &PredefinedMenuItem::about(None, Some(about_metadata)),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::services(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::hide(None),
                &PredefinedMenuItem::hide_others(None),
                &PredefinedMenuItem::show_all(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::quit(None),
            ])
            .expect("build app menu");

        let file_menu = Submenu::new("File", true);
        menu.append(&file_menu).expect("append file menu");
        let new_file = MenuItem::new("New", true, Some(Accelerator::new(Some(Modifiers::SUPER), Code::KeyN)));
        file_menu.append(&new_file).expect("append new file");
        actions.insert(new_file.id().0.clone(), MenuAction::NewFile);
        let open = MenuItem::new("Open...", true, Some(Accelerator::new(Some(Modifiers::SUPER), Code::KeyO)));
        file_menu.append(&open).expect("append open");
        actions.insert(open.id().0.clone(), MenuAction::OpenFile);

        let save = MenuItem::new("Save", false, Some(Accelerator::new(Some(Modifiers::SUPER), Code::KeyS)));
        let save_as = MenuItem::new(
            "Save As...",
            false,
            Some(Accelerator::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyS)),
        );
        file_menu.append(&save).expect("append save");
        file_menu.append(&save_as).expect("append save as");
        actions.insert(save.id().0.clone(), MenuAction::Save);
        actions.insert(save_as.id().0.clone(), MenuAction::SaveAs);
        let save_items = vec![save, save_as];
        file_menu.append(&PredefinedMenuItem::separator()).expect("append separator");
        let close_tab = MenuItem::new("Close", true, Some(Accelerator::new(Some(Modifiers::SUPER), Code::KeyW)));
        file_menu.append(&close_tab).expect("append close tab");
        actions.insert(close_tab.id().0.clone(), MenuAction::CloseTab);

        let edit_menu = Submenu::new("Edit", true);
        menu.append(&edit_menu).expect("append edit menu");

        let undo = MenuItem::new("Undo", false, Some(Accelerator::new(Some(Modifiers::SUPER), Code::KeyZ)));
        let redo =
            MenuItem::new("Redo", false, Some(Accelerator::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyZ)));
        edit_menu.append(&undo).expect("append undo");
        edit_menu.append(&redo).expect("append redo");
        actions.insert(undo.id().0.clone(), MenuAction::Undo);
        actions.insert(redo.id().0.clone(), MenuAction::Redo);
        let undo_items = vec![undo];
        let redo_items = vec![redo];
        edit_menu.append(&PredefinedMenuItem::separator()).expect("append separator");

        let toggle_edit =
            MenuItem::new("Toggle Edit Mode", false, Some(Accelerator::new(Some(Modifiers::SUPER), Code::KeyE)));
        edit_menu.append(&toggle_edit).expect("append toggle edit");
        actions.insert(toggle_edit.id().0.clone(), MenuAction::ToggleEditMode);
        let edit_mode_items = vec![toggle_edit];
        edit_menu.append(&PredefinedMenuItem::separator()).expect("append separator");

        let paste = MenuItem::new("Paste", false, Some(Accelerator::new(Some(Modifiers::SUPER), Code::KeyV)));
        let paste_as_hex = MenuItem::new(
            "Paste as hex",
            false,
            Some(Accelerator::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyV)),
        );
        edit_menu.append(&paste).expect("append paste");
        edit_menu.append(&paste_as_hex).expect("append paste as hex");
        actions.insert(paste.id().0.clone(), MenuAction::Paste);
        actions.insert(paste_as_hex.id().0.clone(), MenuAction::PasteAsHex);
        let paste_items = vec![paste, paste_as_hex];
        edit_menu.append(&PredefinedMenuItem::separator()).expect("append separator");

        let copy_bytes = MenuItem::new("Copy bytes", false, Some(Accelerator::new(Some(Modifiers::SUPER), Code::KeyC)));
        let copy_hex = MenuItem::new(
            "Copy hex",
            false,
            Some(Accelerator::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyC)),
        );
        edit_menu.append(&copy_bytes).expect("append copy bytes");
        edit_menu.append(&copy_hex).expect("append copy hex");
        actions.insert(copy_bytes.id().0.clone(), MenuAction::CopyBytes);
        actions.insert(copy_hex.id().0.clone(), MenuAction::CopyHex);

        let copy_as = Submenu::new("Copy bytes as", false);
        edit_menu.append(&copy_as).expect("append copy as");
        let mut bytes_items = vec![copy_bytes.clone(), copy_hex.clone()];
        for (label, kind) in crate::files::copy::BYTES_MENU {
            let item = MenuItem::new(*label, false, None);
            copy_as.append(&item).expect("append copy-as entry");
            actions.insert(item.id().0.clone(), MenuAction::CopyAs(*kind));
            bytes_items.push(item);
        }

        let copy_value_as = Submenu::new("Copy value as", false);
        edit_menu.append(&copy_value_as).expect("append copy value as");
        let mut scalar_items = Vec::new();
        for (label, kind) in crate::files::copy::VALUE_MENU {
            let item = MenuItem::new(*label, false, None);
            copy_value_as.append(&item).expect("append copy-value-as entry");
            actions.insert(item.id().0.clone(), MenuAction::CopyAs(*kind));
            scalar_items.push(item);
        }

        let view_menu = Submenu::new("View", true);
        menu.append(&view_menu).expect("append view menu");
        let console = MenuItem::new("Toggle Console", true, None);
        let inspector = MenuItem::new("Toggle Inspector", true, None);
        let plugins = MenuItem::new("Toggle Plugins", true, None);
        view_menu.append(&console).expect("append console");
        view_menu.append(&inspector).expect("append inspector");
        view_menu.append(&plugins).expect("append plugins");
        actions.insert(console.id().0.clone(), MenuAction::ToggleConsole);
        actions.insert(inspector.id().0.clone(), MenuAction::ToggleInspector);
        actions.insert(plugins.id().0.clone(), MenuAction::TogglePlugins);

        menu.init_for_nsapp();

        Self {
            actions,
            bytes_items,
            scalar_items,
            save_items,
            edit_mode_items,
            undo_items,
            redo_items,
            paste_items,
            _menu: menu,
        }
    }

    /// Toggle the Save / Save As entries' enabled state.
    pub fn set_save_enabled(&self, enabled: bool) {
        for item in &self.save_items {
            item.set_enabled(enabled);
        }
    }

    pub fn set_edit_mode_enabled(&self, enabled: bool) {
        for item in &self.edit_mode_items {
            item.set_enabled(enabled);
        }
    }

    pub fn set_undo_enabled(&self, enabled: bool) {
        for item in &self.undo_items {
            item.set_enabled(enabled);
        }
    }

    pub fn set_redo_enabled(&self, enabled: bool) {
        for item in &self.redo_items {
            item.set_enabled(enabled);
        }
    }

    pub fn set_paste_enabled(&self, enabled: bool) {
        for item in &self.paste_items {
            item.set_enabled(enabled);
        }
    }

    /// Grey out / enable the byte-copy items. Called once per frame
    /// from the app update loop with `has_active_file`.
    pub fn set_file_open(&self, has_file: bool) {
        for item in &self.bytes_items {
            item.set_enabled(has_file);
        }
    }

    /// Grey out / enable the scalar value-copy items (only meaningful
    /// when the selection is 1/2/4/8 bytes wide).
    pub fn set_scalar_selection(&self, has_scalar: bool) {
        for item in &self.scalar_items {
            item.set_enabled(has_scalar);
        }
    }

    /// Drain any pending `muda` events and return the mapped actions.
    /// Unknown ids are silently dropped -- they belong to predefined
    /// items (Quit, Hide, Undo, ...) that the OS handles itself.
    pub fn drain_actions(&self) -> Vec<MenuAction> {
        let mut out = Vec::new();
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if let Some(action) = self.actions.get(&event.id.0) {
                out.push(*action);
            }
        }
        out
    }
}

/// Kill AppKit's automatic window tabbing. If left at the default
/// `automatic` mode, AppKit injects "Show Tab Bar" and "Merge All
/// Windows" into whichever menu it thinks is the window menu -- which
/// is noise for a single-window app.
#[allow(unsafe_code)]
fn disable_automatic_window_tabbing() {
    use objc2::msg_send;
    use objc2::runtime::AnyClass;
    let Some(cls) = AnyClass::get(c"NSWindow") else { return };
    // SAFETY: sending `setAllowsAutomaticWindowTabbing:` (a class method on
    // NSWindow taking BOOL) is safe once AppKit is loaded; muda calls AppKit
    // from the same thread via init_for_nsapp right after this.
    unsafe {
        let _: () = msg_send![cls, setAllowsAutomaticWindowTabbing: false];
    }
}
