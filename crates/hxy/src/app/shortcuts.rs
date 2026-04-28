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
#[cfg(not(target_arch = "wasm32"))]
pub fn dispatch_save_shortcut(ctx: &egui::Context, app: &mut HxyApp) {
    use crate::commands::shortcuts::NEW_FILE;
    use crate::commands::shortcuts::REDO;
    use crate::commands::shortcuts::SAVE_FILE;
    use crate::commands::shortcuts::SAVE_FILE_AS;
    use crate::commands::shortcuts::TOGGLE_EDIT_MODE;
    use crate::commands::shortcuts::UNDO;

    let (new_file, save_as, save, toggle, redo, undo) = ctx.input_mut(|i| {
        (
            i.consume_shortcut(&NEW_FILE),
            i.consume_shortcut(&SAVE_FILE_AS),
            i.consume_shortcut(&SAVE_FILE),
            i.consume_shortcut(&TOGGLE_EDIT_MODE),
            i.consume_shortcut(&REDO),
            i.consume_shortcut(&UNDO),
        )
    });
    if new_file {
        crate::files::new::handle_new_file(app);
    }
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

#[cfg(target_arch = "wasm32")]
pub fn dispatch_save_shortcut(_ctx: &egui::Context, _app: &mut HxyApp) {}

/// Clipboard paste dispatcher. Consumes Cmd+V and Cmd+Shift+V plus any
/// matching `Event::Paste` eframe auto-generated, reads the clipboard
/// through `arboard`, parses as hex when the shift variant fired, and
/// writes the result at the active tab's cursor.
#[cfg(not(target_arch = "wasm32"))]
pub fn dispatch_paste_shortcut(ctx: &egui::Context, app: &mut HxyApp) {
    use crate::app::ConsoleSeverity;
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
    let text = match paste_event_text {
        Some(t) if !t.is_empty() => t,
        _ => match crate::files::paste::read_text() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "read clipboard");
                return;
            }
        },
    };
    let bytes = if paste_hex {
        match crate::files::paste::parse_hex_clipboard(&text) {
            Ok(b) => b,
            Err(e) => {
                app.console_log(
                    ConsoleSeverity::Warning,
                    "Paste as hex",
                    format!("clipboard text is not valid hex: {e}"),
                );
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
    paste_bytes_at_cursor(file, bytes);
}

/// Apply a paste buffer at the tab's cursor. Length-preserving: the
/// write is truncated to what fits before EOF, leaves an empty
/// clipboard as a no-op, and parks the caret just past the last
/// written byte so the next paste / keystroke lands after it.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn paste_bytes_at_cursor(file: &mut crate::files::OpenFile, bytes: Vec<u8>) {
    let source_len = file.editor.source().len().get();
    if source_len == 0 {
        return;
    }
    let start = file.editor.selection().map(|s| s.range().start().get()).unwrap_or(0);
    let available = source_len.saturating_sub(start);
    if available == 0 {
        return;
    }
    let n = (bytes.len() as u64).min(available) as usize;
    let bytes = if n == bytes.len() { bytes } else { bytes[..n].to_vec() };
    file.editor.push_history_boundary();
    if let Err(e) = file.editor.request_write(start, bytes) {
        tracing::warn!(error = %e, "paste write");
        return;
    }
    let new_cursor = (start + n as u64).min(source_len.saturating_sub(1));
    file.editor.set_selection(Some(hxy_core::Selection::caret(hxy_core::ByteOffset::new(new_cursor))));
    file.editor.reset_edit_nibble();
    file.editor.push_history_boundary();
}

/// App-level copy shortcut handler. Runs after the dock renders, so
/// per-widget hover-copy (status bar labels) has already had a chance
/// to consume the event. Whatever's left dispatches to the currently
/// active file tab.
pub fn dispatch_copy_shortcut(ctx: &egui::Context, app: &mut HxyApp) {
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
#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
fn toggle_global_search(app: &mut HxyApp) {
    if let Some(path) = app.dock.find_tab(&crate::tabs::Tab::SearchResults) {
        let _ = app.dock.remove_tab(path);
        return;
    }
    app.dock.main_surface_mut().split_below(egui_dock::NodeIndex::root(), 0.65, vec![crate::tabs::Tab::SearchResults]);
    app.global_search.open = true;
}
