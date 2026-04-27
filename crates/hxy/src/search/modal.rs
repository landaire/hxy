//! Modal prompts the search subsystem raises:
//!
//! * splice-prompt for find/replace pairs of different lengths,
//! * "Replace N occurrences?" confirmation for Replace All.
//!
//! [`drain_search_effects`] turns each open file's `pending_effects`
//! queue into the right modal request or toast; [`render_search_modal`]
//! consumes the request next frame and routes confirms through the
//! [`super::replace`] helpers.

#![cfg(not(target_arch = "wasm32"))]

use crate::app::HxyApp;
use crate::files::FileId;

use super::replace::perform_replace_all;
use super::replace::perform_replace_current;

/// Modal request raised by the search handler that the app renders
/// next frame. Distinguishes between the length-mismatch splice
/// prompt (single replace or replace-all) and the Replace-All count
/// confirmation.
pub enum PendingSearchModal {
    /// "This will resize the file" prompt for a single Replace, with
    /// the offset / pattern lengths the user is acknowledging.
    LengthMismatchOnce { file_id: FileId, deferred: crate::search::DeferredReplace },
    /// "This will resize the file" prompt for Replace All; carries
    /// the full match list so confirm can splice without re-scanning.
    LengthMismatchAll { file_id: FileId, deferred: crate::search::DeferredReplaceAll },
    /// "Replace N occurrences?" confirmation modal for Replace All.
    ConfirmReplaceAll { file_id: FileId, deferred: crate::search::DeferredReplaceAll },
}

enum ModalOutcome {
    Confirm,
    Cancel,
    Pending,
}

/// Drain every open file's [`crate::search::SearchState::pending_effects`]
/// queue and turn the entries into toasts / modals.
pub fn drain_search_effects(app: &mut HxyApp) {
    use crate::search::SearchSideEffect;

    let file_ids: Vec<FileId> = app.files.keys().copied().collect();
    for id in file_ids {
        let effects: Vec<SearchSideEffect> = match app.files.get_mut(&id) {
            Some(f) => f.search.pending_effects.drain(..).collect(),
            None => continue,
        };
        for e in effects {
            match e {
                SearchSideEffect::WrappedForward => {
                    app.toasts.info(&hxy_i18n::t("search-wrapped-forward"));
                }
                SearchSideEffect::WrappedBackward => {
                    app.toasts.info(&hxy_i18n::t("search-wrapped-backward"));
                }
                SearchSideEffect::Replaced { count } => {
                    let text = hxy_i18n::t_args("search-replaced-toast", &[("count", &count.to_string())]);
                    app.toasts.success(&text);
                }
                SearchSideEffect::NeedsLengthMismatchAck(deferred) => {
                    if app.pending_search_modal.is_none() {
                        app.pending_search_modal =
                            Some(PendingSearchModal::LengthMismatchOnce { file_id: id, deferred });
                    }
                }
                SearchSideEffect::NeedsReplaceAllConfirm(deferred) => {
                    if app.pending_search_modal.is_none() {
                        app.pending_search_modal =
                            Some(PendingSearchModal::ConfirmReplaceAll { file_id: id, deferred });
                    }
                }
            }
        }
    }
}

/// Render whichever search modal is currently pending (at most one
/// at a time). Confirmation routes back through the replace helpers
/// after setting `splice_prompt_acked` where appropriate.
pub fn render_search_modal(ctx: &egui::Context, app: &mut HxyApp) {
    let Some(modal) = app.pending_search_modal.take() else { return };

    match modal {
        PendingSearchModal::LengthMismatchOnce { file_id, deferred } => {
            let outcome = render_length_mismatch_modal(ctx, deferred.find_len, deferred.replace_len);
            match outcome {
                ModalOutcome::Confirm => {
                    let Some(file) = app.files.get_mut(&file_id) else { return };
                    let (Some(find), Some(repl)) = (file.search.pattern.clone(), file.search.replace_pattern.clone())
                    else {
                        return;
                    };
                    file.search.splice_prompt_acked = true;
                    perform_replace_current(file, deferred.offset, &find, &repl);
                }
                ModalOutcome::Cancel => {}
                ModalOutcome::Pending => {
                    app.pending_search_modal = Some(PendingSearchModal::LengthMismatchOnce { file_id, deferred });
                }
            }
        }
        PendingSearchModal::LengthMismatchAll { file_id, deferred } => {
            let outcome = render_length_mismatch_modal(ctx, deferred.find_len, deferred.replace_len);
            match outcome {
                ModalOutcome::Confirm => {
                    let Some(file) = app.files.get_mut(&file_id) else { return };
                    let Some(repl) = file.search.replace_pattern.clone() else { return };
                    file.search.splice_prompt_acked = true;
                    perform_replace_all(file, &deferred.matches, deferred.find_len, &repl);
                }
                ModalOutcome::Cancel => {}
                ModalOutcome::Pending => {
                    app.pending_search_modal = Some(PendingSearchModal::LengthMismatchAll { file_id, deferred });
                }
            }
        }
        PendingSearchModal::ConfirmReplaceAll { file_id, deferred } => {
            let outcome = render_replace_all_confirm_modal(ctx, deferred.matches.len());
            match outcome {
                ModalOutcome::Confirm => {
                    let Some(file) = app.files.get_mut(&file_id) else { return };
                    let lengths_differ = deferred.find_len != deferred.replace_len;
                    if lengths_differ && !file.search.splice_prompt_acked {
                        app.pending_search_modal = Some(PendingSearchModal::LengthMismatchAll { file_id, deferred });
                        return;
                    }
                    let Some(repl) = file.search.replace_pattern.clone() else { return };
                    perform_replace_all(file, &deferred.matches, deferred.find_len, &repl);
                }
                ModalOutcome::Cancel => {}
                ModalOutcome::Pending => {
                    app.pending_search_modal = Some(PendingSearchModal::ConfirmReplaceAll { file_id, deferred });
                }
            }
        }
    }
}

fn render_length_mismatch_modal(ctx: &egui::Context, find_len: u64, replace_len: u64) -> ModalOutcome {
    let mut outcome = ModalOutcome::Pending;
    egui::Window::new(hxy_i18n::t("search-replace-prompt-title"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.label(hxy_i18n::t_args(
                "search-replace-prompt-body",
                &[("find-len", &find_len.to_string()), ("repl-len", &replace_len.to_string())],
            ));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(hxy_i18n::t("search-replace-prompt-confirm")).clicked() {
                    outcome = ModalOutcome::Confirm;
                }
                if ui.button(hxy_i18n::t("search-replace-prompt-cancel")).clicked() {
                    outcome = ModalOutcome::Cancel;
                }
            });
        });
    outcome
}

fn render_replace_all_confirm_modal(ctx: &egui::Context, count: usize) -> ModalOutcome {
    let mut outcome = ModalOutcome::Pending;
    egui::Window::new(hxy_i18n::t("search-replace-all-confirm-title"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.label(hxy_i18n::t_args("search-replace-all-confirm-body", &[("count", &count.to_string())]));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(hxy_i18n::t("search-replace-all-confirm-yes")).clicked() {
                    outcome = ModalOutcome::Confirm;
                }
                if ui.button(hxy_i18n::t("search-replace-all-confirm-no")).clicked() {
                    outcome = ModalOutcome::Cancel;
                }
            });
        });
    outcome
}
