//! Pure formatting for the shell's window title and bottom status bar.
//!
//! Kept free of gpui/entity state so it's covered by plain unit
//! tests; `workspace.rs` wires these to live editor state.

use std::path::Path;

use hxy_core::Selection;
use hxy_editor::VimMode;

/// Status-bar / title label for the currently open file: its base
/// name, or the i18n "no file" placeholder when nothing is open.
pub fn status_file_name_text(path: Option<&Path>) -> String {
    match path {
        Some(path) => {
            // `file_name()` is `None` for paths ending in `..` or a
            // trailing separator; the full path is still meaningful
            // to show in that case rather than nothing at all.
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string())
        }
        None => hxy_i18n::t("gpui-status-no-file"),
    }
}

/// Window title: "<file> - hxy", or just the app name with no file
/// open.
pub fn window_title_text(path: Option<&Path>) -> String {
    match path {
        Some(path) => hxy_i18n::t_args(
            "gpui-window-title",
            &[("file", &status_file_name_text(Some(path))), ("app", &hxy_i18n::t("app-name"))],
        ),
        None => hxy_i18n::t("app-name"),
    }
}

/// Status-bar label for the cursor offset, and the selection length
/// when the selection is a range rather than a caret.
pub fn status_offset_text(selection: Option<Selection>) -> String {
    match selection {
        None => hxy_i18n::t("gpui-status-no-selection"),
        Some(sel) if sel.is_caret() => hxy_i18n::t_args("gpui-status-offset", &[("offset", &sel.cursor.to_string())]),
        Some(sel) => {
            let len = sel.range().len().get();
            hxy_i18n::t_args("gpui-status-selection", &[("offset", &sel.cursor.to_string()), ("len", &len.to_string())])
        }
    }
}

/// Status-bar label for the vim sub-mode. Callers only show this
/// while vim input mode is active.
pub fn status_vim_mode_text(vim_mode: VimMode) -> String {
    let key = match vim_mode {
        VimMode::Normal => "gpui-status-vim-mode-normal",
        VimMode::Visual => "gpui-status-vim-mode-visual",
        VimMode::VisualLine => "gpui-status-vim-mode-visual-line",
        VimMode::Insert => "gpui-status-vim-mode-insert",
        VimMode::Replace => "gpui-status-vim-mode-replace",
    };
    format!("[{}]", hxy_i18n::t(key))
}

/// Dirty marker appended to the status bar when the buffer has
/// unsaved edits, empty otherwise. Not routed through i18n: it's a
/// punctuation glyph, not natural-language text.
pub fn dirty_marker(is_dirty: bool) -> &'static str {
    if is_dirty { "*" } else { "" }
}

/// Status-bar label for a failed file-open attempt (CLI or dialog).
pub fn status_open_error_text(path: &Path, error: &str) -> String {
    hxy_i18n::t_args("gpui-status-open-error", &[("path", &path.display().to_string()), ("error", error)])
}

#[cfg(test)]
mod tests {
    use hxy_core::ByteOffset;

    use super::*;

    #[test]
    fn file_name_text_none_is_placeholder() {
        assert_eq!(status_file_name_text(None), hxy_i18n::t("gpui-status-no-file"));
    }

    #[test]
    fn file_name_text_some_uses_base_name() {
        assert_eq!(status_file_name_text(Some(Path::new("/a/b/dump.bin"))), "dump.bin");
    }

    #[test]
    fn file_name_text_falls_back_to_full_path_without_a_file_name() {
        assert_eq!(status_file_name_text(Some(Path::new("/a/b/.."))), Path::new("/a/b/..").display().to_string());
    }

    #[test]
    fn window_title_no_file_is_app_name() {
        assert_eq!(window_title_text(None), hxy_i18n::t("app-name"));
    }

    #[test]
    fn window_title_some_file_appends_app_name() {
        let title = window_title_text(Some(Path::new("/a/dump.bin")));
        assert!(title.contains("dump.bin"));
        assert!(title.contains(&hxy_i18n::t("app-name")));
    }

    #[test]
    fn offset_text_none_selection() {
        assert_eq!(status_offset_text(None), hxy_i18n::t("gpui-status-no-selection"));
    }

    #[test]
    fn offset_text_caret_has_no_length_suffix() {
        let sel = Selection::caret(ByteOffset::new(0x2A));
        let text = status_offset_text(Some(sel));
        assert!(text.contains("0x2A"));
        assert!(!text.contains("byte"));
    }

    #[test]
    fn offset_text_range_includes_length() {
        let sel = Selection { anchor: ByteOffset::new(0x10), cursor: ByteOffset::new(0x1F) };
        let text = status_offset_text(Some(sel));
        assert!(text.contains("0x1F"));
        assert!(text.contains("16"));
    }

    #[test]
    fn vim_mode_text_is_bracketed() {
        let text = status_vim_mode_text(VimMode::Insert);
        assert!(text.starts_with('['));
        assert!(text.ends_with(']'));
    }

    #[test]
    fn dirty_marker_reflects_flag() {
        assert_eq!(dirty_marker(true), "*");
        assert_eq!(dirty_marker(false), "");
    }

    #[test]
    fn open_error_text_mentions_path_and_error() {
        let text = status_open_error_text(Path::new("/a/dump.bin"), "permission denied");
        assert!(text.contains("dump.bin"));
        assert!(text.contains("permission denied"));
    }
}
