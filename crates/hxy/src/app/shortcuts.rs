//! Per-frame keyboard shortcut dispatchers. Each one consumes
//! exactly the egui input events it owns and routes the action to
//! the appropriate subsystem (file save, paste, copy, search, ...).

use crate::app::HxyApp;
use crate::commands::shortcuts::COPY_HEX;
use crate::files::copy::CopyKind;

/// App-level keypress -> nibble write + arrow-key cursor navigation
/// dispatcher. Runs late in the frame so other widgets (palette
/// text input, settings fields, dialogs) get first crack at typed
/// keys via egui's normal focus path; only un-consumed presses
/// reach the active hex-edit cursor.
pub fn dispatch_hex_edit_keys(ctx: &egui::Context, app: &mut HxyApp) {
    let Some(id) = crate::app::active_file_id(app) else { return };
    if let Some(file) = app.files.get_mut(&id) {
        file.editor.handle_input(ctx);
    }
}

/// Cmd+] / Cmd+[ jump the caret to the next / previous template
/// field. No-op when the active file has no template loaded so the
/// shortcut is reserved but inert -- matches the disabled palette
/// entries' behavior.
#[cfg(not(target_arch = "wasm32"))]
pub fn dispatch_jump_field_shortcut(ctx: &egui::Context, app: &mut HxyApp) {
    use crate::commands::shortcuts::JUMP_NEXT_FIELD;
    use crate::commands::shortcuts::JUMP_PREV_FIELD;

    let (next, prev) = ctx.input_mut(|i| (i.consume_shortcut(&JUMP_NEXT_FIELD), i.consume_shortcut(&JUMP_PREV_FIELD)));
    if next {
        crate::app::jump_to_template_field(app, true);
    }
    if prev {
        crate::app::jump_to_template_field(app, false);
    }
}

/// New-file / save / save-as / toggle-edit-mode / undo / redo
/// shortcuts. All consumed in one input borrow so a Cmd+Shift+S
/// doesn't bleed into the bare Cmd+S handler.
pub fn dispatch_save_shortcut(ctx: &egui::Context, app: &mut HxyApp) {
    use crate::commands::shortcuts::NEW_FILE;
    use crate::commands::shortcuts::REDO;
    #[cfg(not(target_arch = "wasm32"))]
    use crate::commands::shortcuts::SAVE_FILE;
    #[cfg(not(target_arch = "wasm32"))]
    use crate::commands::shortcuts::SAVE_FILE_AS;
    use crate::commands::shortcuts::TOGGLE_EDIT_MODE;
    use crate::commands::shortcuts::UNDO;

    let (new_file, toggle, redo, undo) = ctx.input_mut(|i| {
        (
            i.consume_shortcut(&NEW_FILE),
            i.consume_shortcut(&TOGGLE_EDIT_MODE),
            i.consume_shortcut(&REDO),
            i.consume_shortcut(&UNDO),
        )
    });
    #[cfg(not(target_arch = "wasm32"))]
    let (save_as, save) = ctx.input_mut(|i| (i.consume_shortcut(&SAVE_FILE_AS), i.consume_shortcut(&SAVE_FILE)));
    if new_file {
        #[cfg(not(target_arch = "wasm32"))]
        crate::files::new::handle_new_file(app);
        #[cfg(target_arch = "wasm32")]
        app.open_bytes_wasm("Untitled".to_owned(), Vec::new());
    }
    #[cfg(not(target_arch = "wasm32"))]
    if save_as {
        crate::files::save::save_active_file(app, true);
    } else if save {
        crate::files::save::save_active_file(app, false);
    }
    if toggle {
        crate::app::toggle_active_edit_mode(app);
    }
    if redo {
        crate::app::redo_active_file(app);
    } else if undo {
        crate::app::undo_active_file(app);
    }
}

/// Clipboard paste dispatcher. Consumes Cmd+V and Cmd+Shift+V plus any
/// matching `Event::Paste` eframe auto-generated, reads the clipboard
/// through `arboard`, parses as hex when the shift variant fired, and
/// writes the result at the active tab's cursor.
pub fn dispatch_paste_shortcut(ctx: &egui::Context, app: &mut HxyApp) {
    use crate::commands::shortcuts::PASTE;
    use crate::commands::shortcuts::PASTE_AS_HEX;

    if ctx.egui_wants_keyboard_input() {
        return;
    }
    let (paste, paste_hex, paste_event_text) = ctx.input_mut(|i| {
        let paste_hex = i.consume_shortcut(&PASTE_AS_HEX);
        let paste = i.consume_shortcut(&PASTE);
        let mut event_text = None;
        i.events.retain(|event| {
            if let egui::Event::Paste(text) = event
                && event_text.is_none()
            {
                event_text = Some(text.clone());
                return false;
            }
            true
        });
        (paste, paste_hex, event_text)
    });
    if !paste && !paste_hex {
        return;
    }
    let Some(id) = crate::app::active_file_id(app) else { return };
    let Some(file) = app.files.get_mut(&id) else { return };
    if file.editor.edit_mode() != crate::files::EditMode::Mutable {
        return;
    }
    // Desktop falls back to arboard when there's no Event::Paste --
    // happens with explicit Cmd+V from the menu / shortcut while
    // egui already has focus on a non-editable widget. On wasm
    // arboard isn't available; egui delivers every Cmd+V as a
    // Paste event already, so an empty event_text just means "no
    // clipboard text to paste".
    let text = match paste_event_text {
        Some(t) if !t.is_empty() => t,
        _ => {
            #[cfg(not(target_arch = "wasm32"))]
            match crate::files::paste::read_text() {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(error = %e, "read clipboard");
                    return;
                }
            }
            #[cfg(target_arch = "wasm32")]
            {
                return;
            }
        }
    };
    let bytes = if paste_hex {
        match crate::files::paste::parse_hex_clipboard(&text) {
            Ok(b) => b,
            Err(e) => {
                #[cfg(not(target_arch = "wasm32"))]
                app.console_log(
                    crate::app::ConsoleSeverity::Warning,
                    "Paste as hex",
                    format!("clipboard text is not valid hex: {e}"),
                );
                #[cfg(target_arch = "wasm32")]
                tracing::warn!(error = %e, "paste as hex");
                let _ = app;
                return;
            }
        }
    } else {
        text.into_bytes()
    };
    if bytes.is_empty() {
        return;
    }
    let Some(file) = app.files.get_mut(&id) else { return };
    paste_bytes_at_cursor(&mut file.editor, bytes);
}

/// Apply a paste buffer at the editor's cursor. Bytes that fit
/// before EOF overwrite in place; the rest append, so pasting into
/// an empty anonymous buffer or past the last byte grows the source.
/// Empty clipboards no-op. The caret parks just past the last
/// written byte so the next paste / keystroke lands after it.
pub(crate) fn paste_bytes_at_cursor(editor: &mut hxy_view::HexEditor, bytes: Vec<u8>) {
    if bytes.is_empty() {
        return;
    }
    let source_len = editor.source().len().get();
    let start = editor.selection().map(|s| s.range().start().get()).unwrap_or(0).min(source_len);
    let n = bytes.len() as u64;
    let overwrite = n.min(source_len.saturating_sub(start));
    editor.push_history_boundary();
    if let Err(e) = editor.splice(start, overwrite, bytes) {
        tracing::warn!(error = %e, "paste splice");
        return;
    }
    let new_cursor = start + n;
    editor.set_selection(Some(hxy_core::Selection::caret(hxy_core::ByteOffset::new(new_cursor))));
    editor.reset_edit_nibble();
    editor.push_history_boundary();
}

/// App-level copy shortcut handler. Runs after the dock renders, so
/// per-widget hover-copy (status bar labels) has already had a chance
/// to consume the event. Whatever's left dispatches to the currently
/// active file tab.
pub fn dispatch_copy_shortcut(ctx: &egui::Context, app: &mut HxyApp) {
    // egui's selectable Labels and TextEdits handle Cmd+C through
    // [`egui::Event::Copy`] and queue an [`egui::OutputCommand::CopyText`]
    // for the integration to ship to the system clipboard. They
    // don't bother consuming the matching key-press from
    // [`egui::InputState::events`], so the hex view's dispatcher
    // would happily grab it next and overwrite the clipboard with
    // bytes from the active file. Bail out when egui already
    // queued a clipboard write so the user's selected label / row
    // text is what actually lands.
    let already_copying = ctx.output(|o| {
        o.commands.iter().any(|c| matches!(c, egui::OutputCommand::CopyText(_) | egui::OutputCommand::CopyImage(_)))
    });
    if already_copying {
        return;
    }
    let kind = ctx.input_mut(|i| {
        if i.consume_shortcut(&COPY_HEX) {
            Some(CopyKind::BytesHexSpaced)
        } else if consume_copy_event(i) {
            Some(CopyKind::BytesLossyUtf8)
        } else {
            None
        }
    });
    let Some(kind) = kind else { return };
    let Some(id) = crate::app::active_file_id(app) else { return };
    if let Some(file) = app.files.get(&id) {
        crate::app::do_copy(ctx, file, kind);
    }
}

/// Consume the plain "copy" shortcut in all the forms the integration
/// might deliver it: as an `Event::Copy` (winit on macOS converts Cmd+C
/// to a semantic copy event), or as a normal `Event::Key` with the
/// Command modifier on platforms that pass it through.
pub fn consume_copy_event(input: &mut egui::InputState) -> bool {
    use crate::commands::shortcuts::COPY_BYTES;

    // winit on macOS sends Cmd+C as BOTH an `Event::Copy` (the
    // semantic copy) AND a regular Cmd+C `Event::Key`. A previous
    // version of this function returned after draining the semantic
    // form, which left the Key event for the hex-view's dispatcher
    // to grab -- so the status-bar label would copy its value and
    // the hex view would immediately overwrite the clipboard with
    // the current selection. Drain BOTH so a single "copy" click
    // produces one clipboard write.
    let mut any = false;
    let before = input.events.len();
    input.events.retain(|e| !matches!(e, egui::Event::Copy));
    if input.events.len() != before {
        any = true;
    }
    if input.consume_shortcut(&COPY_BYTES) {
        any = true;
    }
    any
}

/// Cmd+F opens / closes the active file tab's search bar; Cmd+Shift+F
/// opens the cross-file search results tab.
pub fn dispatch_find_shortcut(ctx: &egui::Context, app: &mut HxyApp) {
    use crate::commands::shortcuts::FIND_GLOBAL;
    use crate::commands::shortcuts::FIND_LOCAL;

    let global = ctx.input_mut(|i| i.consume_shortcut(&FIND_GLOBAL));
    let local = !global && ctx.input_mut(|i| i.consume_shortcut(&FIND_LOCAL));
    if global {
        toggle_global_search(app);
        return;
    }
    if local {
        toggle_local_search(app);
    }
}

fn toggle_local_search(app: &mut HxyApp) {
    let Some(id) = crate::app::active_file_id(app) else { return };
    let Some(file) = app.files.get_mut(&id) else { return };
    file.search.open = !file.search.open;
    if file.search.open {
        if let Some(sel) = file.editor.selection()
            && !sel.is_caret()
        {
            let r = sel.range();
            file.search.scope =
                crate::search::SearchScope::Selection { start: r.start().get(), end_exclusive: r.end().get() };
        } else {
            file.search.scope = crate::search::SearchScope::File;
        }
        file.search.refresh_pattern();
        file.search.refresh_replace_pattern();
    }
}

pub(crate) fn toggle_global_search(app: &mut HxyApp) {
    if let Some(path) = app.dock.find_tab(&crate::tabs::Tab::SearchResults) {
        let _ = app.dock.remove_tab(path);
        return;
    }
    app.dock.main_surface_mut().split_below(egui_dock::NodeIndex::root(), 0.65, vec![crate::tabs::Tab::SearchResults]);
    app.global_search.open = true;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hxy_core::ByteOffset;
    use hxy_core::ByteRange;
    use hxy_core::HexSource;
    use hxy_core::MemorySource;
    use hxy_core::Selection;
    use hxy_view::HexEditor;

    use super::paste_bytes_at_cursor;

    fn editor_with(bytes: &[u8], cursor: u64) -> HexEditor {
        let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes.to_vec()));
        let mut ed = HexEditor::new(source);
        ed.set_selection(Some(Selection::caret(ByteOffset::new(cursor))));
        ed
    }

    fn read_all(ed: &HexEditor) -> Vec<u8> {
        let len = ed.source().len().get();
        if len == 0 {
            return Vec::new();
        }
        let r = ByteRange::new(ByteOffset::new(0), ByteOffset::new(len)).unwrap();
        ed.source().read(r).unwrap()
    }

    #[test]
    fn paste_into_empty_buffer_grows_buffer() {
        // Pasting into a fresh "Untitled" tab (zero bytes) used to
        // no-op because the length-preserving write rejected an
        // empty source; users had to type a byte first to "unlock"
        // paste. The buffer should grow to hold the pasted bytes
        // and the caret should park just past the last one.
        let mut ed = editor_with(&[], 0);
        paste_bytes_at_cursor(&mut ed, vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(read_all(&ed), vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(ed.selection().unwrap().cursor.get(), 4);
    }

    #[test]
    fn paste_past_eof_extends_buffer() {
        // Caret on the trailing EOF cell of a non-empty buffer
        // should append, not no-op.
        let mut ed = editor_with(&[0x11, 0x22], 2);
        paste_bytes_at_cursor(&mut ed, vec![0x33, 0x44]);
        assert_eq!(read_all(&ed), vec![0x11, 0x22, 0x33, 0x44]);
        assert_eq!(ed.selection().unwrap().cursor.get(), 4);
    }

    #[test]
    fn paste_straddling_eof_overwrites_then_appends() {
        // Partial overlap: the first N bytes overwrite in place,
        // the rest extend past EOF.
        let mut ed = editor_with(&[0x11, 0x22, 0x33, 0x44], 2);
        paste_bytes_at_cursor(&mut ed, vec![0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(read_all(&ed), vec![0x11, 0x22, 0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(ed.selection().unwrap().cursor.get(), 6);
    }

    #[test]
    fn paste_in_bounds_still_overwrites() {
        // Length-preserving overwrite still works for the common
        // case where paste fits entirely inside the buffer.
        let mut ed = editor_with(&[0x11, 0x22, 0x33, 0x44, 0x55], 1);
        paste_bytes_at_cursor(&mut ed, vec![0xAA, 0xBB]);
        assert_eq!(read_all(&ed), vec![0x11, 0xAA, 0xBB, 0x44, 0x55]);
        assert_eq!(ed.selection().unwrap().cursor.get(), 3);
    }

    #[test]
    fn paste_empty_clipboard_is_noop() {
        let mut ed = editor_with(&[0x11, 0x22], 1);
        paste_bytes_at_cursor(&mut ed, Vec::new());
        assert_eq!(read_all(&ed), vec![0x11, 0x22]);
        assert_eq!(ed.selection().unwrap().cursor.get(), 1);
    }
}
