//! Replace-current and Replace-All staging + execution.
//!
//! Both flows route through `pending_effects` so the
//! length-mismatch / Replace-All confirm modals can be raised
//! before any bytes are spliced.

#![cfg(not(target_arch = "wasm32"))]

use crate::files::OpenFile;

/// Stage a Replace-Current. The active match offset comes from the
/// editor's current selection (set when the user last hit Find Next /
/// Prev / clicked an All-Results entry). Defers via
/// [`crate::search::SearchSideEffect::NeedsLengthMismatchAck`] when
/// the find/replace pair would resize the file and the user hasn't
/// acked the splice prompt yet; otherwise performs the replace inline
/// and re-runs Next.
pub fn queue_replace_current(file: &mut OpenFile) {
    use crate::search::DeferredReplace;
    use crate::search::SearchSideEffect;

    let (Some(find), Some(repl)) = (file.search.pattern.clone(), file.search.replace_pattern.clone()) else {
        return;
    };
    let Some(sel) = file.editor.selection() else { return };
    let offset = sel.anchor.get().min(sel.cursor.get());
    let length = sel.range().len().get();
    if length != find.len() as u64 {
        return;
    }
    if find.len() != repl.len() && !file.search.splice_prompt_acked {
        file.search.pending_effects.push(SearchSideEffect::NeedsLengthMismatchAck(DeferredReplace {
            offset,
            find_len: find.len() as u64,
            replace_len: repl.len() as u64,
        }));
        return;
    }
    perform_replace_current(file, offset, &find, &repl);
}

/// Stage a Replace-All. Reads every match in `bounds`, then either
/// queues the count-confirm modal (first time) or performs the
/// splices (after both modals have been acked).
pub fn queue_replace_all(file: &mut OpenFile, bounds: hxy_core::ByteRange) {
    use crate::search::DeferredReplaceAll;
    use crate::search::SearchSideEffect;
    use crate::search::find_all;

    let (Some(find), Some(repl)) = (file.search.pattern.clone(), file.search.replace_pattern.clone()) else {
        return;
    };
    let matches = find_all(file.editor.source().as_ref(), &find, bounds);
    if matches.is_empty() {
        return;
    }
    file.search.pending_effects.push(SearchSideEffect::NeedsReplaceAllConfirm(DeferredReplaceAll {
        matches,
        find_len: find.len() as u64,
        replace_len: repl.len() as u64,
    }));
}

/// Splice / overwrite a single match at `offset`. Caller is
/// responsible for the splice-prompt acknowledgement when sizes
/// differ. Bumps the search refresh after so the All-Results list
/// reflects the new layout.
pub fn perform_replace_current(file: &mut OpenFile, offset: u64, find: &[u8], repl: &[u8]) {
    use crate::search::SearchSideEffect;

    let result = if find.len() == repl.len() {
        file.editor.request_write(offset, repl.to_vec()).map(|_| ())
    } else {
        file.editor.splice(offset, find.len() as u64, repl.to_vec()).map(|_| ())
    };
    if let Err(e) = result {
        tracing::warn!(error = %e, "replace");
        return;
    }
    file.search.pending_effects.push(SearchSideEffect::Replaced { count: 1 });
    let next_offset = offset + repl.len() as u64;
    file.editor.set_selection(Some(hxy_core::Selection {
        anchor: hxy_core::ByteOffset::new(next_offset),
        cursor: hxy_core::ByteOffset::new(next_offset),
    }));
    file.search.refresh_pattern();
    file.search.splice_prompt_acked = true;
}

/// Apply every match in `matches` as a single batched splice so
/// the whole Replace All counts as one undo entry. `matches` must
/// be sorted ascending; the batch is non-overlapping by
/// construction (each match consumes `find_len` bytes). Used after
/// both the count-confirm and (when sizes differ) the splice
/// prompt have been acknowledged.
pub fn perform_replace_all(file: &mut OpenFile, matches: &[u64], find_len: u64, repl: &[u8]) {
    use crate::search::SearchSideEffect;

    if matches.is_empty() {
        return;
    }
    let ops: Vec<(u64, u64, Vec<u8>)> = matches.iter().map(|off| (*off, find_len, repl.to_vec())).collect();
    if let Err(e) = file.editor.splice_many(&ops) {
        tracing::warn!(error = %e, "replace-all batch");
        return;
    }
    file.search.pending_effects.push(SearchSideEffect::Replaced { count: matches.len() });
    file.search.refresh_pattern();
    file.search.splice_prompt_acked = true;
}
