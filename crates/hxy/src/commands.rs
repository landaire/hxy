//! Command pattern for the global toolbar.
//!
//! Each button in the top toolbar is a [`ToolbarCommand`]. Commands
//! inspect a [`ToolbarCtx`] built once per frame to decide their own
//! enabled state and perform their action. This keeps the toolbar
//! itself a dumb renderer: no view-specific state lives there.

use crate::file::FileId;
use crate::file::OpenFile;
use crate::state::PersistedState;

/// Per-frame context handed to every [`ToolbarCommand`]. Holds borrows
/// to the pieces of application state a command might touch.
pub struct ToolbarCtx<'a, 'ctx> {
    pub ctx: &'ctx egui::Context,
    pub state: &'a mut PersistedState,
    pub active_file: Option<&'a mut OpenFile>,
    pub active_file_id: Option<FileId>,
    /// Commands push their side-effect intentions here; the app drains
    /// this at the end of the toolbar pass and applies them.
    pub effects: &'a mut Vec<CommandEffect>,
}

/// Deferred side-effects a command wants to apply. Returned via
/// [`ToolbarCtx::effects`] so commands don't need mutable access to the
/// whole `HxyApp`; the top-level update loop drains them.
#[derive(Debug)]
pub enum CommandEffect {
    OpenFileDialog,
    MountActiveFile,
    OpenRecent(std::path::PathBuf),
    RunTemplateDialog,
    /// Run a specific template file without prompting -- pushed when
    /// the auto-detected library has matched the active file.
    RunTemplateDirect(std::path::PathBuf),
}

/// Button in the global toolbar. Commands are registered once at
/// startup; a new capability is a new `impl` of this trait plus a
/// registration.
pub trait ToolbarCommand: Send + Sync {
    /// Stable id used for diffing, logging, and shortcut lookup.
    fn id(&self) -> &'static str;

    /// Human label for the button tooltip. Icon-only buttons still
    /// carry a label for accessibility.
    fn label(&self, cx: &ToolbarCtx<'_, '_>) -> String;

    /// Phosphor icon (rendered via egui-phosphor fonts).
    fn icon(&self) -> &'static str;

    /// Whether the button should be clickable this frame.
    fn enabled(&self, cx: &ToolbarCtx<'_, '_>) -> bool;

    /// Optional keyboard shortcut displayed next to the label.
    fn shortcut(&self) -> Option<egui::KeyboardShortcut> {
        None
    }

    /// Invoked when the user clicks the button.
    fn invoke(&self, cx: &mut ToolbarCtx<'_, '_>);
}

/// Built-in command: open a file via the native file dialog.
pub struct OpenFileCommand;

impl ToolbarCommand for OpenFileCommand {
    fn id(&self) -> &'static str {
        "open-file"
    }

    fn label(&self, _: &ToolbarCtx<'_, '_>) -> String {
        hxy_i18n::t("toolbar-open-file")
    }

    fn icon(&self) -> &'static str {
        egui_phosphor::regular::FOLDER_OPEN
    }

    fn enabled(&self, _: &ToolbarCtx<'_, '_>) -> bool {
        true
    }

    fn invoke(&self, cx: &mut ToolbarCtx<'_, '_>) {
        cx.effects.push(CommandEffect::OpenFileDialog);
    }
}

/// Built-in command: mount the active file's detected VFS and open the
/// tree panel. Enabled only when the active tab has a handler match.
pub struct BrowseArchiveCommand;

impl ToolbarCommand for BrowseArchiveCommand {
    fn id(&self) -> &'static str {
        "browse-archive"
    }

    fn label(&self, _: &ToolbarCtx<'_, '_>) -> String {
        hxy_i18n::t("toolbar-browse-archive")
    }

    fn icon(&self) -> &'static str {
        egui_phosphor::regular::TREE_STRUCTURE
    }

    fn enabled(&self, cx: &ToolbarCtx<'_, '_>) -> bool {
        cx.active_file.as_ref().is_some_and(|f| f.detected_handler.is_some())
    }

    fn invoke(&self, cx: &mut ToolbarCtx<'_, '_>) {
        cx.effects.push(CommandEffect::MountActiveFile);
    }
}

/// Built-in command: run a template (`.bt`, `.hexpat`, ...) against the
/// active file. Opens a file picker; the extension routes the template
/// to the matching runtime.
pub struct RunTemplateCommand;

impl ToolbarCommand for RunTemplateCommand {
    fn id(&self) -> &'static str {
        "run-template"
    }

    fn label(&self, cx: &ToolbarCtx<'_, '_>) -> String {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(name) =
            cx.active_file.as_ref().and_then(|f| f.suggested_template.as_ref()).map(|s| s.display_name.clone())
        {
            return format!("Run {name}");
        }
        let _ = cx;
        hxy_i18n::t("toolbar-run-template")
    }

    fn icon(&self) -> &'static str {
        egui_phosphor::regular::SCROLL
    }

    fn enabled(&self, cx: &ToolbarCtx<'_, '_>) -> bool {
        cx.active_file.is_some()
    }

    fn invoke(&self, cx: &mut ToolbarCtx<'_, '_>) {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(path) = cx.active_file.as_ref().and_then(|f| f.suggested_template.as_ref()).map(|s| s.path.clone())
        {
            cx.effects.push(CommandEffect::RunTemplateDirect(path));
            return;
        }
        cx.effects.push(CommandEffect::RunTemplateDialog);
    }
}

/// Default command list registered at app startup.
pub fn default_commands() -> Vec<Box<dyn ToolbarCommand>> {
    vec![Box::new(OpenFileCommand), Box::new(BrowseArchiveCommand), Box::new(RunTemplateCommand)]
}
