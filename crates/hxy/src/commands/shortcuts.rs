//! Keyboard shortcut table.
//!
//! All the `egui::KeyboardShortcut` constants the app dispatches on
//! live here. The corresponding `dispatch_*_shortcut` functions and
//! the palette `with_shortcut` calls reference these by name; one
//! place to grep when you want to know "what does Cmd+X do" or
//! "what's free to bind."
//!
//! Naming convention: the constant matches the action the shortcut
//! performs (e.g. `SAVE_FILE` is what `Cmd+S` does). Modifiers are
//! `COMMAND` rather than `CTRL` whenever the binding is the
//! conventional macOS Cmd / non-macOS Ctrl pair; egui maps that to
//! the platform-specific modifier automatically.

use egui::Key;
use egui::KeyboardShortcut;
use egui::Modifiers;

// -------- Editing --------

pub const COPY_BYTES: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::C);
pub const COPY_HEX: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::C);
pub const PASTE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::V);
pub const PASTE_AS_HEX: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::V);
pub const UNDO: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Z);
pub const REDO: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::Z);
pub const TOGGLE_EDIT_MODE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::E);

// -------- Files / tabs --------

pub const NEW_FILE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::N);
pub const CLOSE_TAB: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::W);
pub const SAVE_FILE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
pub const SAVE_FILE_AS: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::S);

// -------- Tab navigation --------

/// Cycle to the next tab in the active dock surface (outer or inner
/// workspace dock, depending on `TabFocus`).
pub const NEXT_TAB: KeyboardShortcut = KeyboardShortcut::new(Modifiers::CTRL, Key::Tab);
/// Reverse of [`NEXT_TAB`].
pub const PREV_TAB: KeyboardShortcut = KeyboardShortcut::new(Modifiers::CTRL.plus(Modifiers::SHIFT), Key::Tab);
/// Alt+Tab swaps which dock `Ctrl+Tab` targets (outer dock vs the
/// active workspace's inner dock). On macOS `Modifiers::ALT` is the
/// Option key.
pub const TOGGLE_TAB_FOCUS: KeyboardShortcut = KeyboardShortcut::new(Modifiers::ALT, Key::Tab);

// -------- Palette --------

/// Cmd+P opens the filename-first quick-open list (open files
/// plus recent paths). Re-pressing while the same mode is already
/// open closes the palette.
pub const QUICK_OPEN: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::P);
/// Cmd+Shift+P opens the full command palette (commands, files,
/// templates, plugin contributions). Re-pressing closes it.
pub const COMMAND_PALETTE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::P);

// -------- Pane focus --------

/// Cmd+K starts the visual pane-focus picker -- every leaf gets a
/// letter overlay and pressing one snaps focus there. Inspired by
/// wezterm / zellij's pane-jump bindings.
pub const FOCUS_PANE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::K);

// -------- Search --------

pub const FIND_LOCAL: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::F);
pub const FIND_GLOBAL: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::F);
