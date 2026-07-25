//! The GPUI shell's root view: hosts the [`HexPane`], the bottom
//! status bar, file-open (CLI + `cmd-o` dialog), the `cmd-alt-v` vim
//! toggle, live system-theme sync, and the window title.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::App;
use gpui::Context;
use gpui::Entity;
use gpui::FocusHandle;
use gpui::Focusable;
use gpui::InteractiveElement;
use gpui::IntoElement;
use gpui::ParentElement;
use gpui::PathPromptOptions;
use gpui::Render;
use gpui::Styled;
use gpui::Subscription;
use gpui::Window;
use gpui::actions;
use gpui::div;
use gpui::prelude::*;
use gpui_component::ActiveTheme;
use gpui_component::h_flex;
use gpui_component::label::Label;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_editor::InputMode;
use hxy_view_gpui::HexPane;

use crate::status::dirty_marker;
use crate::status::status_file_name_text;
use crate::status::status_offset_text;
use crate::status::status_open_error_text;
use crate::status::status_vim_mode_text;
use crate::status::window_title_text;

actions!(hxy_gpui, [OpenFile, ToggleVim]);

/// Register the shell's keybindings. Called once at startup before any
/// window opens.
pub fn init_keybindings(cx: &mut App) {
    cx.bind_keys([gpui::KeyBinding::new("cmd-o", OpenFile, None), gpui::KeyBinding::new("cmd-alt-v", ToggleVim, None)]);
}

/// A file read from the CLI path argument or the open dialog, ready
/// to become (or replace) the pane's source.
struct OpenedFile {
    source: Arc<dyn HexSource>,
    path: PathBuf,
}

/// A failed attempt to read a file chosen via the open dialog.
struct OpenFileError {
    path: PathBuf,
    error: std::io::Error,
}

fn read_file(path: PathBuf) -> Result<OpenedFile, OpenFileError> {
    match std::fs::read(&path) {
        Ok(bytes) => Ok(OpenedFile { source: Arc::new(MemorySource::new(bytes)), path }),
        Err(error) => Err(OpenFileError { path, error }),
    }
}

pub struct Workspace {
    pane: Option<Entity<HexPane>>,
    file_path: Option<PathBuf>,
    /// Message for the most recent failed open attempt (CLI or
    /// dialog); cleared on the next successful open.
    open_error: Option<String>,
    last_title: Option<String>,
    /// Tracked on the root div so `cmd-o` / `cmd-alt-v` stay
    /// reachable even before any pane exists to click into: gpui
    /// dispatches actions along the path from the focused element up
    /// to the window root, and falls back to the root alone when
    /// nothing is focused at all. Without an ancestor of the action
    /// listeners holding focus by default, the two shortcuts would be
    /// dead on a fresh launch with no file open. Once a pane exists,
    /// `render` moves keyboard focus onto it instead (see
    /// `focus_pending`) -- `HexPane`, as a descendant of this div,
    /// keeps the same action reachability while also letting
    /// keystrokes reach `HexPane::on_key_down` immediately, with no
    /// click required first.
    focus_handle: FocusHandle,
    /// Set on construction and whenever a pane is created or its
    /// source is replaced. Consumed on the next `render`, which has
    /// the `&mut Window` needed to move focus (window creation and
    /// the async open-dialog task that creates or replaces a pane do
    /// not have one) -- assigns the pane's handle if one exists,
    /// otherwise falls back to the workspace's own handle so `cmd-o`
    /// stays reachable with no file open.
    focus_pending: bool,
    _appearance_subscription: Subscription,
}

impl Workspace {
    /// `initial` is the CLI path argument's already-read bytes, if
    /// any; `appearance_subscription` keeps the live system-theme
    /// observer alive for the workspace's lifetime.
    pub fn new(initial: Option<(Arc<dyn HexSource>, PathBuf)>, appearance_subscription: Subscription, cx: &mut Context<Self>) -> Self {
        let (pane, file_path) = match initial {
            Some((source, path)) => (Some(cx.new(|cx| HexPane::new(source, cx))), Some(path)),
            None => (None, None),
        };
        Self {
            pane,
            file_path,
            open_error: None,
            last_title: None,
            focus_handle: cx.focus_handle(),
            focus_pending: true,
            _appearance_subscription: appearance_subscription,
        }
    }

    fn on_open_file(&mut self, _: &OpenFile, _window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: None });
        cx.spawn(async move |this, cx| {
            let result = receiver.await;
            let _ = this.update(cx, |workspace, cx| {
                match result {
                    Ok(Ok(Some(mut paths))) => {
                        // `multiple: false` above; at most one path.
                        if let Some(path) = paths.pop() {
                            workspace.apply_open_result(read_file(path), cx);
                        }
                    }
                    Ok(Ok(None)) => {
                        // User cancelled the dialog; normal no-op.
                    }
                    Ok(Err(err)) => {
                        tracing::error!(%err, "file picker failed");
                        workspace.open_error = Some(hxy_i18n::t_args("gpui-status-open-error-dialog", &[("error", &err.to_string())]));
                        cx.notify();
                    }
                    Err(_) => {
                        // Channel dropped (window closing); nothing to show.
                    }
                }
            });
        })
        .detach();
    }

    fn apply_open_result(&mut self, result: Result<OpenedFile, OpenFileError>, cx: &mut Context<Self>) {
        match result {
            Ok(opened) => {
                match &self.pane {
                    Some(pane) => pane.update(cx, |pane, cx| pane.set_source(opened.source, cx)),
                    None => self.pane = Some(cx.new(|cx| HexPane::new(opened.source, cx))),
                }
                self.file_path = Some(opened.path);
                self.open_error = None;
                self.focus_pending = true;
            }
            Err(OpenFileError { path, error }) => {
                tracing::error!(?path, %error, "failed to open file");
                self.open_error = Some(status_open_error_text(&path, &error.to_string()));
            }
        }
        cx.notify();
    }

    fn on_toggle_vim(&mut self, _: &ToggleVim, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.pane.clone() else { return };
        pane.update(cx, |pane, cx| {
            let next = match pane.editor().input_mode() {
                InputMode::Default => InputMode::Vim,
                InputMode::Vim => InputMode::Default,
            };
            pane.editor_mut().set_input_mode(next);
            cx.notify();
        });
    }

    fn render_status_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let file_label = Label::new(status_file_name_text(self.file_path.as_deref()));

        let middle = match &self.pane {
            Some(pane) => {
                let editor = pane.read(cx).editor();
                Label::new(status_offset_text(editor.selection()))
            }
            None => Label::new(String::new()),
        };

        let right = match &self.pane {
            Some(pane) => {
                let editor = pane.read(cx).editor();
                let mut text = String::new();
                if matches!(editor.input_mode(), InputMode::Vim) {
                    text.push_str(&status_vim_mode_text(editor.vim_state().mode));
                    text.push(' ');
                }
                text.push_str(dirty_marker(editor.is_dirty()));
                Label::new(text)
            }
            None => Label::new(String::new()),
        };

        h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .gap_3()
            .px_3()
            .py_1()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .text_color(cx.theme().muted_foreground)
            .child(file_label)
            .child(middle)
            .child(right)
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = window_title_text(self.file_path.as_deref());
        if self.last_title.as_deref() != Some(title.as_str()) {
            window.set_window_title(&title);
            self.last_title = Some(title);
        }

        if self.focus_pending {
            let handle = match &self.pane {
                Some(pane) => pane.read(cx).focus_handle(cx),
                None => self.focus_handle.clone(),
            };
            window.focus(&handle);
            self.focus_pending = false;
        }

        let body = match &self.pane {
            Some(pane) => div().flex_1().child(pane.clone()),
            None => div().flex_1().flex().items_center().justify_center().child(hxy_i18n::t("gpui-shell-no-file")),
        };

        let mut root = div()
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .on_action(cx.listener(Self::on_open_file))
            .on_action(cx.listener(Self::on_toggle_vim))
            .child(body);

        if let Some(error) = &self.open_error {
            root = root.child(div().px_3().py_1().text_color(cx.theme().danger).child(error.clone()));
        }

        root.child(self.render_status_bar(cx))
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use hxy_core::HexSource;
    use hxy_core::MemorySource;

    use super::*;

    fn source() -> Arc<dyn HexSource> {
        Arc::new(MemorySource::new(vec![0u8; 16]))
    }

    /// Regression test for a launch-time bug: focus was given to
    /// `Workspace` even when a CLI file had already loaded a pane, so
    /// keystrokes needed a click into the grid before they reached
    /// `HexPane::on_key_down`. `Workspace::render` now moves focus
    /// onto the pane itself once one exists (see `focus_pending`).
    #[gpui::test]
    fn cli_open_focuses_the_pane_not_the_workspace(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            let subscription = window.observe_window_appearance(|_, _| {});
            Workspace::new(Some((source(), PathBuf::from("test.bin"))), subscription, cx)
        });

        let pane_handle = workspace.read_with(cx, |ws, cx| ws.pane.as_ref().unwrap().read(cx).focus_handle(cx));
        let workspace_handle = workspace.read_with(cx, |ws, cx| ws.focus_handle(cx));
        cx.update(|window, cx| {
            let focused = window.focused(cx);
            assert_eq!(focused, Some(pane_handle));
            assert_ne!(focused, Some(workspace_handle));
        });
    }

    /// With no file open there is nothing to focus the pane onto;
    /// `cmd-o` / `cmd-alt-v` must still be reachable, so focus stays
    /// on `Workspace`'s own handle.
    #[gpui::test]
    fn no_file_focuses_the_workspace(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            let subscription = window.observe_window_appearance(|_, _| {});
            Workspace::new(None, subscription, cx)
        });

        let workspace_handle = workspace.read_with(cx, |ws, cx| ws.focus_handle(cx));
        cx.update(|window, cx| {
            assert_eq!(window.focused(cx), Some(workspace_handle));
        });
    }

    /// Regression test for the `cmd-o` half of the same bug: applying
    /// a successful open result (what the dialog's async callback
    /// does once the user picks a file) must also move focus onto
    /// the pane, whether it was freshly created or reused via
    /// `set_source` on an already-open pane.
    #[gpui::test]
    fn open_result_focuses_the_pane(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            let subscription = window.observe_window_appearance(|_, _| {});
            Workspace::new(None, subscription, cx)
        });

        workspace.update(cx, |ws, cx| {
            ws.apply_open_result(Ok(OpenedFile { source: source(), path: PathBuf::from("opened.bin") }), cx);
        });
        cx.run_until_parked();

        let pane_handle = workspace.read_with(cx, |ws, cx| ws.pane.as_ref().unwrap().read(cx).focus_handle(cx));
        cx.update(|window, cx| {
            assert_eq!(window.focused(cx), Some(pane_handle));
        });
    }
}
