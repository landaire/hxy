//! [`SearchBar`]: the in-file find/replace bar rendered below a
//! [`FilePanel`](super::FilePanel)'s [`HexPane`].
//!
//! Query encoding, match scanning, and the shared [`SearchState`] all
//! live in `hxy_panels::search` (framework-agnostic, shared with the
//! egui front end's `crates/hxy/src/search` module). This file only
//! owns the two [`InputState`]s, wires their events to `SearchState`,
//! and applies the results onto the file's [`HexEditor`] -- the gpui
//! counterpart of `crates/hxy/src/search/{bar,replace,modal}.rs`.
//!
//! Match highlighting is selection-jump only (egui parity): a match
//! sets the editor selection to the match range and scrolls it into
//! view, with no separate byte-level highlight layer.
//!
//! Replace-All's ">1 match" confirm and the length-mismatch splice
//! warning are real [`WindowExt::open_dialog`] dialogs (unlike egui's
//! next-frame modal queue, gpui-component's dialog layer renders
//! immediately). Wrap-around and replace-count both still push onto
//! `SearchState::pending_effects` (kept for observability / parity with
//! the egui bar's queue), and are additionally surfaced immediately as
//! a [`WindowExt::push_notification`] toast at the same call site --
//! see [`Self::next_match`] / [`Self::prev_match`] /
//! [`Self::perform_replace_current`] / [`Self::perform_replace_all`].

use gpui::App;
use gpui::AppContext;
use gpui::Context;
use gpui::Entity;
use gpui::FocusHandle;
use gpui::Focusable;
use gpui::InteractiveElement;
use gpui::IntoElement;
use gpui::ParentElement;
use gpui::Render;
use gpui::Styled;
use gpui::Subscription;
use gpui::Window;
use gpui::div;
use gpui::prelude::FluentBuilder;
use gpui::px;
use gpui_component::ActiveTheme;
use gpui_component::Disableable;
use gpui_component::IconName;
use gpui_component::Selectable;
use gpui_component::WindowExt;
use gpui_component::button::Button;
use gpui_component::checkbox::Checkbox;
use gpui_component::h_flex;
use gpui_component::input::Input;
use gpui_component::input::InputEvent;
use gpui_component::input::InputState;
use gpui_component::label::Label;
use gpui_component::notification::Notification;
use gpui_component::v_flex;
use hxy_core::ByteOffset;
use hxy_core::HexSource;
use hxy_core::Selection;
use hxy_editor::HexEditor;
use hxy_panels::search::Endian;
use hxy_panels::search::NumberWidth;
use hxy_panels::search::SearchKind;
use hxy_panels::search::SearchSideEffect;
use hxy_panels::search::SearchState;
use hxy_view_gpui::HexPane;

/// Query/replace inputs plus the shared [`SearchState`] for one file's
/// search bar. Owns a handle to the file's [`HexPane`] so it can read
/// the source, move the selection, and splice bytes directly.
pub struct SearchBar {
    pane: Entity<HexPane>,
    state: SearchState,
    query_input: Entity<InputState>,
    replace_input: Entity<InputState>,
    _query_sub: Subscription,
    _replace_sub: Subscription,
}

impl SearchBar {
    pub fn new(pane: Entity<HexPane>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query_input = cx.new(|cx| InputState::new(window, cx));
        let replace_input = cx.new(|cx| InputState::new(window, cx));
        let query_sub = cx.subscribe_in(&query_input, window, Self::on_query_event);
        let replace_sub = cx.subscribe_in(&replace_input, window, Self::on_replace_event);
        Self {
            pane,
            state: SearchState::default(),
            query_input,
            replace_input,
            _query_sub: query_sub,
            _replace_sub: replace_sub,
        }
    }

    pub fn is_open(&self) -> bool {
        self.state.open
    }

    /// Live editor handle, for tests that need to assert on bytes /
    /// selection / undo stack after a bar operation.
    #[cfg(test)]
    pub(crate) fn pane(&self) -> &Entity<HexPane> {
        &self.pane
    }

    #[cfg(test)]
    pub(crate) fn state(&self) -> &SearchState {
        &self.state
    }

    /// Open the bar (re-deriving the find/replace patterns from
    /// whatever query is still there from a prior toggle) and focus
    /// the query field.
    fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.state.open = true;
        self.state.refresh_pattern();
        self.state.refresh_replace_pattern();
        self.query_input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    /// Close the bar and hand keyboard focus back to the grid.
    pub fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.state.open = false;
        let handle = self.pane.read(cx).focus_handle(cx);
        window.focus(&handle);
        cx.notify();
    }

    pub fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.open {
            self.close(window, cx);
        } else {
            self.open(window, cx);
        }
    }

    fn on_query_event(
        &mut self,
        _input: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => {
                self.state.query = self.query_input.read(cx).value().to_string();
                self.refresh(cx);
                cx.notify();
            }
            // `secondary` is cmd/ctrl-Enter, not shift-Enter: 0.5.1's
            // `InputState` only distinguishes those two, so Prev rides
            // the secondary-Enter binding rather than egui's shift-Enter.
            InputEvent::PressEnter { secondary: false } => self.next_match(window, cx),
            InputEvent::PressEnter { secondary: true } => self.prev_match(window, cx),
            InputEvent::Focus | InputEvent::Blur => {}
        }
    }

    fn on_replace_event(&mut self, _input: &Entity<InputState>, event: &InputEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if let InputEvent::Change = event {
            self.state.replace_query = self.replace_input.read(cx).value().to_string();
            self.state.refresh_replace_pattern();
            cx.notify();
        }
    }

    /// Re-derive both patterns and, if the results list is showing,
    /// recompute it. Called after any kind/width/signed/endian change
    /// (both patterns share those settings) and after the query text
    /// changes.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.state.refresh_pattern();
        self.state.refresh_replace_pattern();
        if self.state.all_results {
            self.recompute_all_results(cx);
        }
    }

    fn set_kind(&mut self, kind: SearchKind, cx: &mut Context<Self>) {
        self.state.kind = kind;
        self.refresh(cx);
        cx.notify();
    }

    fn set_width(&mut self, width: NumberWidth, cx: &mut Context<Self>) {
        self.state.width = width;
        self.refresh(cx);
        cx.notify();
    }

    fn set_signed(&mut self, signed: bool, cx: &mut Context<Self>) {
        self.state.signed = signed;
        self.refresh(cx);
        cx.notify();
    }

    fn set_endian(&mut self, endian: Endian, cx: &mut Context<Self>) {
        self.state.endian = endian;
        self.refresh(cx);
        cx.notify();
    }

    fn toggle_replace(&mut self, cx: &mut Context<Self>) {
        self.state.replace_open = !self.state.replace_open;
        cx.notify();
    }

    fn set_all_results(&mut self, on: bool, cx: &mut Context<Self>) {
        self.state.all_results = on;
        if on {
            self.recompute_all_results(cx);
            if let (Some(idx), Some(pattern)) = (self.state.active_idx, self.state.pattern.clone()) {
                let off = self.state.matches[idx];
                self.apply_match_jump(off, &pattern, cx);
            }
        } else {
            self.state.matches.clear();
            self.state.active_idx = None;
        }
        cx.notify();
    }

    fn recompute_all_results(&mut self, cx: &mut Context<Self>) {
        let Some(pattern) = self.state.pattern.clone() else {
            self.state.matches.clear();
            self.state.active_idx = None;
            return;
        };
        let pane = self.pane.read(cx);
        let editor = pane.editor();
        let bounds = self.state.scope.bounds(editor.source().len().get());
        self.state.matches = hxy_panels::search::find_all(editor.source().as_ref(), &pattern, bounds);
        self.state.active_idx = nearest_match_idx(&self.state.matches, current_caret(editor));
    }

    /// Find the next match after the caret, wrapping past EOF. Mirrors
    /// `crates/hxy/src/app/mod.rs`'s `SearchEvent::Next` arm.
    fn next_match(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pattern) = self.state.pattern.clone() else { return };
        let hit = {
            let pane = self.pane.read(cx);
            let editor = pane.editor();
            let bounds = self.state.scope.bounds(editor.source().len().get());
            let from = current_caret(editor).saturating_add(1);
            hxy_panels::search::find_next(editor.source().as_ref(), &pattern, from, true, bounds)
        };
        let Some(hit) = hit else { return };
        self.apply_match_jump(hit.offset, &pattern, cx);
        if hit.wrapped {
            self.state.pending_effects.push(SearchSideEffect::WrappedForward);
            window.push_notification(Notification::info(hxy_i18n::t("search-wrapped-forward")), cx);
        }
        cx.notify();
    }

    /// Find the previous match before the caret, wrapping past offset
    /// 0. Mirrors `SearchEvent::Prev`.
    fn prev_match(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pattern) = self.state.pattern.clone() else { return };
        let hit = {
            let pane = self.pane.read(cx);
            let editor = pane.editor();
            let bounds = self.state.scope.bounds(editor.source().len().get());
            let from = current_caret(editor);
            hxy_panels::search::find_prev(editor.source().as_ref(), &pattern, from, true, bounds)
        };
        let Some(hit) = hit else { return };
        self.apply_match_jump(hit.offset, &pattern, cx);
        if hit.wrapped {
            self.state.pending_effects.push(SearchSideEffect::WrappedBackward);
            window.push_notification(Notification::info(hxy_i18n::t("search-wrapped-backward")), cx);
        }
        cx.notify();
    }

    /// Set the editor selection to the match range and scroll it into
    /// view. Mirrors `apply_match_jump` in `crates/hxy/src/app/mod.rs`:
    /// selection-only highlight, no separate byte styler.
    fn apply_match_jump(&mut self, off: u64, pattern: &[u8], cx: &mut Context<Self>) {
        let end_inclusive = off.saturating_add(pattern.len() as u64).saturating_sub(1);
        let pane = self.pane.clone();
        pane.update(cx, |pane, cx| {
            pane.editor_mut().set_selection(Some(Selection { anchor: ByteOffset::new(off), cursor: ByteOffset::new(end_inclusive) }));
            pane.editor_mut().set_scroll_to_byte(ByteOffset::new(off));
            pane.sync_pending_scroll(cx);
        });
        if let Ok(idx) = self.state.matches.binary_search(&off) {
            self.state.active_idx = Some(idx);
        }
    }

    /// Stage a Replace-Current. Mirrors
    /// `crates/hxy/src/search/replace.rs::queue_replace_current`: when
    /// the find/replace lengths differ and the splice prompt hasn't
    /// been acked yet, opens the length-mismatch confirm dialog instead
    /// of writing immediately.
    fn queue_replace_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(find), Some(repl)) = (self.state.pattern.clone(), self.state.replace_pattern.clone()) else { return };
        let Some(sel) = self.pane.read(cx).editor().selection() else { return };
        let offset = sel.anchor.get().min(sel.cursor.get());
        let length = sel.range().len().get();
        if length != find.len() as u64 {
            return;
        }
        if find.len() != repl.len() && !self.state.splice_prompt_acked {
            self.open_length_mismatch_dialog(offset, find.len() as u64, repl.len() as u64, window, cx);
            return;
        }
        self.perform_replace_current(offset, &find, &repl, window, cx);
    }

    /// Overwrite (or splice, if the length changed) the match at
    /// `offset`. Mirrors `replace::perform_replace_current`.
    fn perform_replace_current(
        &mut self,
        offset: u64,
        find: &[u8],
        repl: &[u8],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self.pane.clone();
        let result = pane.update(cx, |pane, cx| {
            let editor = pane.editor_mut();
            let result =
                if find.len() == repl.len() { editor.request_write(offset, repl.to_vec()) } else { editor.splice(offset, find.len() as u64, repl.to_vec()) };
            if result.is_ok() {
                let next_offset = offset + repl.len() as u64;
                editor.set_selection(Some(Selection::caret(ByteOffset::new(next_offset))));
            }
            cx.notify();
            result
        });
        match result {
            Ok(()) => {
                self.state.pending_effects.push(SearchSideEffect::Replaced { count: 1 });
                self.state.refresh_pattern();
                self.state.splice_prompt_acked = true;
                if self.state.all_results {
                    self.recompute_all_results(cx);
                }
                let text = hxy_i18n::t_args("search-replaced-toast", &[("count", "1")]);
                window.push_notification(Notification::success(text), cx);
                // The `cx.notify()` inside `pane.update` above repaints
                // the `HexPane` entity; the bar's own status label /
                // button-disabled state also changed and needs its own.
                cx.notify();
            }
            Err(error) => tracing::warn!(%error, "replace"),
        }
    }

    /// Stage a Replace-All. Mirrors `replace::queue_replace_all`, with
    /// one intentional divergence from egui's `modal.rs`: a single
    /// match skips the ">1 match" count-confirm dialog and goes
    /// straight to the length-mismatch check (egui always shows the
    /// count-confirm, even for one match) -- a one-match "Replace 1
    /// occurrence?" prompt adds a click with nothing to weigh, so the
    /// gpui bar only asks when there's more than one candidate.
    fn queue_replace_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(find), Some(repl)) = (self.state.pattern.clone(), self.state.replace_pattern.clone()) else { return };
        let matches = {
            let pane = self.pane.read(cx);
            let editor = pane.editor();
            let bounds = self.state.scope.bounds(editor.source().len().get());
            hxy_panels::search::find_all(editor.source().as_ref(), &find, bounds)
        };
        if matches.is_empty() {
            return;
        }
        let find_len = find.len() as u64;
        let replace_len = repl.len() as u64;
        if matches.len() > 1 {
            self.open_replace_all_confirm(matches, find_len, replace_len, window, cx);
        } else {
            self.continue_replace_all(matches, find_len, replace_len, window, cx);
        }
    }

    fn continue_replace_all(&mut self, matches: Vec<u64>, find_len: u64, replace_len: u64, window: &mut Window, cx: &mut Context<Self>) {
        if find_len != replace_len && !self.state.splice_prompt_acked {
            self.open_length_mismatch_dialog_for_all(matches, find_len, replace_len, window, cx);
            return;
        }
        let Some(repl) = self.state.replace_pattern.clone() else { return };
        self.perform_replace_all(&matches, find_len, &repl, window, cx);
    }

    /// Apply every match as one batched `splice_many`, so the whole
    /// Replace All is a single undo entry. Mirrors
    /// `replace::perform_replace_all`.
    fn perform_replace_all(
        &mut self,
        matches: &[u64],
        find_len: u64,
        repl: &[u8],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self.pane.clone();
        let ops: Vec<(u64, u64, Vec<u8>)> = matches.iter().map(|off| (*off, find_len, repl.to_vec())).collect();
        let result = pane.update(cx, |pane, cx| {
            let result = pane.editor_mut().splice_many(&ops);
            cx.notify();
            result
        });
        match result {
            Ok(()) => {
                let count = matches.len();
                self.state.pending_effects.push(SearchSideEffect::Replaced { count });
                self.state.refresh_pattern();
                self.state.splice_prompt_acked = true;
                if self.state.all_results {
                    self.recompute_all_results(cx);
                }
                let text = hxy_i18n::t_args("search-replaced-toast", &[("count", &count.to_string())]);
                window.push_notification(Notification::success(text), cx);
                cx.notify();
            }
            Err(error) => tracing::warn!(%error, "replace-all batch"),
        }
    }

    fn open_length_mismatch_dialog(&mut self, offset: u64, find_len: u64, replace_len: u64, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity();
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let this = this.clone();
            dialog
                .title(hxy_i18n::t("search-replace-prompt-title"))
                .child(Label::new(hxy_i18n::t_args(
                    "search-replace-prompt-body",
                    &[("find-len", &find_len.to_string()), ("repl-len", &replace_len.to_string())],
                )))
                .confirm()
                .on_ok(move |_, window, cx| {
                    this.update(cx, |bar, cx| {
                        bar.state.splice_prompt_acked = true;
                        let (Some(find), Some(repl)) = (bar.state.pattern.clone(), bar.state.replace_pattern.clone()) else { return };
                        bar.perform_replace_current(offset, &find, &repl, window, cx);
                    });
                    true
                })
        });
    }

    fn open_length_mismatch_dialog_for_all(&mut self, matches: Vec<u64>, find_len: u64, replace_len: u64, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity();
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let this = this.clone();
            let matches = matches.clone();
            dialog
                .title(hxy_i18n::t("search-replace-prompt-title"))
                .child(Label::new(hxy_i18n::t_args(
                    "search-replace-prompt-body",
                    &[("find-len", &find_len.to_string()), ("repl-len", &replace_len.to_string())],
                )))
                .confirm()
                .on_ok(move |_, window, cx| {
                    this.update(cx, |bar, cx| {
                        bar.state.splice_prompt_acked = true;
                        let Some(repl) = bar.state.replace_pattern.clone() else { return };
                        bar.perform_replace_all(&matches, find_len, &repl, window, cx);
                    });
                    true
                })
        });
    }

    fn open_replace_all_confirm(&mut self, matches: Vec<u64>, find_len: u64, replace_len: u64, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity();
        let count = matches.len();
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let this = this.clone();
            let matches = matches.clone();
            dialog
                .title(hxy_i18n::t("search-replace-all-confirm-title"))
                .child(Label::new(hxy_i18n::t_args("search-replace-all-confirm-body", &[("count", &count.to_string())])))
                .confirm()
                .on_ok(move |_, window, cx| {
                    this.update(cx, |bar, cx| bar.continue_replace_all(matches.clone(), find_len, replace_len, window, cx));
                    true
                })
        });
    }

    fn kind_button(&self, kind: SearchKind, label_key: &str, id: &'static str, cx: &Context<Self>) -> Button {
        Button::new(id).label(hxy_i18n::t(label_key)).compact().selected(self.state.kind == kind).on_click(cx.listener(move |this, _, _window, cx| this.set_kind(kind, cx)))
    }

    fn width_button(&self, width: NumberWidth, cx: &Context<Self>) -> Button {
        let id: &'static str = match width {
            NumberWidth::W8 => "search-width-8",
            NumberWidth::W16 => "search-width-16",
            NumberWidth::W32 => "search-width-32",
            NumberWidth::W64 => "search-width-64",
        };
        let bits = (width.bytes() * 8).to_string();
        Button::new(id)
            .label(hxy_i18n::t_args("search-number-width", &[("bits", &bits)]))
            .compact()
            .selected(self.state.width == width)
            .on_click(cx.listener(move |this, _, _window, cx| this.set_width(width, cx)))
    }

    fn endian_button(&self, endian: Endian, label_key: &str, id: &'static str, cx: &Context<Self>) -> Button {
        Button::new(id)
            .label(hxy_i18n::t(label_key))
            .compact()
            .selected(self.state.endian == endian)
            .on_click(cx.listener(move |this, _, _window, cx| this.set_endian(endian, cx)))
    }

    fn render_status(&self, cx: &Context<Self>) -> gpui::AnyElement {
        if let Some(err) = &self.state.error {
            return Label::new(err.clone()).text_color(cx.theme().danger).into_any_element();
        }
        if self.state.all_results {
            let total = self.state.matches.len();
            let text = match self.state.active_idx {
                Some(i) => hxy_i18n::t_args("search-status-active-of-total", &[("index", &(i + 1).to_string()), ("total", &total.to_string())]),
                None => hxy_i18n::t_args("search-status-match-count", &[("count", &total.to_string())]),
            };
            return Label::new(text).text_color(cx.theme().muted_foreground).into_any_element();
        }
        if self.state.pattern.is_some() {
            return Label::new(hxy_i18n::t("search-status-press-enter")).text_color(cx.theme().muted_foreground).into_any_element();
        }
        div().into_any_element()
    }

    fn render_find_row(&self, cx: &Context<Self>) -> impl IntoElement {
        let mut row = h_flex()
            .gap_2()
            .items_center()
            .child(Label::new(hxy_i18n::t("search-find-label")))
            .child(self.kind_button(SearchKind::Text, "search-kind-text", "search-kind-text-btn", cx))
            .child(self.kind_button(SearchKind::HexBytes, "search-kind-hex-bytes", "search-kind-hex-bytes-btn", cx))
            .child(self.kind_button(SearchKind::Number, "search-kind-number", "search-kind-number-btn", cx));

        if matches!(self.state.kind, SearchKind::Number) {
            row = row
                .child(self.width_button(NumberWidth::W8, cx))
                .child(self.width_button(NumberWidth::W16, cx))
                .child(self.width_button(NumberWidth::W32, cx))
                .child(self.width_button(NumberWidth::W64, cx))
                .child(
                    Checkbox::new("search-signed")
                        .label(hxy_i18n::t("search-signed"))
                        .checked(self.state.signed)
                        .on_click(cx.listener(|this, checked: &bool, _window, cx| this.set_signed(*checked, cx))),
                )
                .child(self.endian_button(Endian::Little, "search-endian-little", "search-endian-little-btn", cx))
                .child(self.endian_button(Endian::Big, "search-endian-big", "search-endian-big-btn", cx));
        }

        let replace_toggle_label =
            if self.state.replace_open { hxy_i18n::t("search-replace-toggle-hide") } else { hxy_i18n::t("search-replace-toggle-show") };

        row.child(div().w(px(220.0)).child(Input::new(&self.query_input)))
            .child(
                Button::new("search-next")
                    .icon(IconName::ChevronDown)
                    .tooltip(hxy_i18n::t("search-next-tooltip"))
                    .compact()
                    .on_click(cx.listener(|this, _, window, cx| this.next_match(window, cx))),
            )
            .child(
                Button::new("search-prev")
                    .icon(IconName::ChevronUp)
                    // gpui-specific key: 0.5.1's `InputState` only
                    // distinguishes plain vs. cmd/ctrl-Enter, not
                    // shift-Enter (egui's binding), so the hint text
                    // differs from the shared `search-prev-tooltip`.
                    .tooltip(hxy_i18n::t("gpui-search-prev-tooltip"))
                    .compact()
                    .on_click(cx.listener(|this, _, window, cx| this.prev_match(window, cx))),
            )
            .child(
                Checkbox::new("search-all-results")
                    .label(hxy_i18n::t("search-all-results"))
                    .checked(self.state.all_results)
                    .on_click(cx.listener(|this, checked: &bool, _window, cx| this.set_all_results(*checked, cx))),
            )
            .child(
                Button::new("search-replace-toggle")
                    .label(replace_toggle_label)
                    .tooltip(hxy_i18n::t("search-replace-toggle-tooltip"))
                    .compact()
                    .on_click(cx.listener(|this, _, _window, cx| this.toggle_replace(cx))),
            )
            .child(self.render_status(cx))
            .child(
                Button::new("search-close")
                    .icon(IconName::Close)
                    .tooltip(hxy_i18n::t("search-close-tooltip"))
                    .compact()
                    .on_click(cx.listener(|this, _, window, cx| this.close(window, cx))),
            )
    }

    fn render_replace_row(&self, cx: &Context<Self>) -> impl IntoElement {
        let can_replace = self.state.pattern.is_some() && self.state.replace_pattern.is_some();
        h_flex()
            .gap_2()
            .items_center()
            .child(Label::new(hxy_i18n::t("search-replace-label")))
            .child(div().w(px(220.0)).child(Input::new(&self.replace_input)))
            .child(
                Button::new("search-replace-once")
                    .label(hxy_i18n::t("search-replace-once"))
                    .tooltip(hxy_i18n::t("search-replace-once-tooltip"))
                    .compact()
                    .disabled(!can_replace)
                    .on_click(cx.listener(|this, _, window, cx| this.queue_replace_current(window, cx))),
            )
            .child(
                Button::new("search-replace-all")
                    .label(hxy_i18n::t("search-replace-all"))
                    .tooltip(hxy_i18n::t("search-replace-all-tooltip"))
                    .compact()
                    .disabled(!can_replace)
                    .on_click(cx.listener(|this, _, window, cx| this.queue_replace_all(window, cx))),
            )
            .when_some(self.state.replace_error.clone(), |row, err| row.child(Label::new(err).text_color(cx.theme().danger)))
    }
}

/// Current caret offset, or the start of the file when there's no
/// selection yet -- matches `crates/hxy/src/app/mod.rs::current_caret`,
/// so an unstarted search begins at offset 0 like the egui app.
fn current_caret(editor: &HexEditor) -> u64 {
    editor.selection().map(|s| s.cursor.get()).unwrap_or(0)
}

/// Index of the match at or immediately after `caret`, clamped to the
/// last match. Mirrors `crates/hxy/src/app/mod.rs::nearest_match_idx`.
fn nearest_match_idx(matches: &[u64], caret: u64) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    Some(matches.partition_point(|&m| m < caret).min(matches.len() - 1))
}

impl Focusable for SearchBar {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query_input.read(cx).focus_handle(cx)
    }
}

impl Render for SearchBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("SearchBar")
            .gap_1()
            .p_2()
            .bg(cx.theme().secondary)
            .border_t_1()
            .border_color(cx.theme().border)
            .child(self.render_find_row(cx))
            .when(self.state.replace_open, |bar| bar.child(self.render_replace_row(cx)))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::TestAppContext;
    use hxy_core::ByteRange;
    use hxy_core::MemorySource;

    use super::*;

    fn setup(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
    }

    fn source(bytes: Vec<u8>) -> Arc<dyn HexSource> {
        Arc::new(MemorySource::new(bytes))
    }

    /// Minimal stand-in for `Workspace::render`'s job of appending the
    /// dialog layer: production only does this once, at the workspace
    /// root, so `SearchBar` itself must stay layer-free. Tests that
    /// drive a real dialog confirm need a host that plays that role.
    struct DialogTestHost {
        bar: Entity<SearchBar>,
    }

    impl Render for DialogTestHost {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.bar.clone()).children(gpui_component::Root::render_dialog_layer(window, cx))
        }
    }

    /// Builds a `SearchBar` inside a real `gpui_component::Root` window
    /// (like the production shell does in `main.rs`), with the dialog
    /// layer wired up like `Workspace::render` does, so
    /// `WindowExt::open_dialog` -- used by the replace-all / length-
    /// mismatch confirms -- both has a layer to attach to and actually
    /// paints/dispatches for tests that drive a real confirm.
    fn build(cx: &mut TestAppContext, bytes: Vec<u8>) -> (Entity<SearchBar>, &mut gpui::VisualTestContext) {
        let window = cx.add_window(|window, cx| {
            let pane = cx.new(|cx| HexPane::new(source(bytes), cx));
            let bar = cx.new(|cx| SearchBar::new(pane, window, cx));
            let host = cx.new(|_cx| DialogTestHost { bar: bar.clone() });
            gpui_component::Root::new(host, window, cx)
        });
        let root = window.root(cx).unwrap();
        let bar = root.read_with(cx, |root, _| root.view().clone().downcast::<DialogTestHost>().unwrap());
        let bar = bar.read_with(cx, |host, _| host.bar.clone());
        let vcx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        vcx.run_until_parked();
        (bar, vcx)
    }

    fn set_query(bar: &Entity<SearchBar>, text: &str, cx: &mut gpui::VisualTestContext) {
        cx.update(|window, cx| {
            bar.update(cx, |bar, cx| {
                bar.query_input.update(cx, |input, cx| input.set_value(text.to_string(), window, cx));
            });
        });
        cx.run_until_parked();
    }

    fn set_replace(bar: &Entity<SearchBar>, text: &str, cx: &mut gpui::VisualTestContext) {
        cx.update(|window, cx| {
            bar.update(cx, |bar, cx| {
                bar.replace_input.update(cx, |input, cx| input.set_value(text.to_string(), window, cx));
            });
        });
        cx.run_until_parked();
    }

    fn selection(bar: &Entity<SearchBar>, cx: &mut gpui::VisualTestContext) -> Option<Selection> {
        bar.read_with(cx, |bar, cx| bar.pane().read(cx).editor().selection())
    }

    fn read_bytes(bar: &Entity<SearchBar>, range: ByteRange, cx: &mut gpui::VisualTestContext) -> Vec<u8> {
        bar.read_with(cx, |bar, cx| bar.pane().read(cx).editor().source().read(range).unwrap())
    }

    /// Typing a hex query and hitting Enter jumps the selection to the
    /// first match and the "N of M" counter reflects it.
    #[gpui::test]
    fn hex_query_enter_jumps_to_first_match_and_updates_counter(cx: &mut TestAppContext) {
        setup(cx);
        let bytes = vec![0u8, 0u8, 0xDE, 0xAD, 0u8, 0xDE, 0xAD, 0u8];
        let (bar, cx) = build(cx, bytes);

        bar.update(cx, |bar, cx| {
            bar.state.kind = SearchKind::HexBytes;
            // Toggling "All Results" on before there's a pattern is a
            // no-op jump-wise (nothing to jump to yet); it just means
            // the match count populates as soon as the query does.
            bar.set_all_results(true, cx);
        });
        set_query(&bar, "DE AD", cx);

        // Enter on the query field routes to `next_match` via the
        // `InputEvent::PressEnter { secondary: false }` subscription.
        cx.update(|window, cx| bar.update(cx, |bar, cx| bar.next_match(window, cx)));

        assert_eq!(selection(&bar, cx), Some(Selection { anchor: ByteOffset::new(2), cursor: ByteOffset::new(3) }));
        let (matches, active_idx) = bar.read_with(cx, |bar, _| (bar.state().matches.clone(), bar.state().active_idx));
        assert_eq!(matches, vec![2, 5]);
        assert_eq!(active_idx, Some(0));
    }

    /// Next wraps past EOF back to the only match, queues a
    /// wrap-forward side effect, and surfaces an info toast through the
    /// `Root` notification layer `DialogTestHost` doesn't even have to
    /// render explicitly -- `push_notification` writes straight into
    /// `Root`'s own state.
    #[gpui::test]
    fn next_wraps_past_eof_and_toasts(cx: &mut TestAppContext) {
        setup(cx);
        let bytes = vec![0xDE, 0xAD, 0u8, 0u8];
        let (bar, cx) = build(cx, bytes);

        bar.update(cx, |bar, _| bar.state.kind = SearchKind::HexBytes);
        set_query(&bar, "DE AD", cx);

        // First Next already wraps: it scans from caret+1 = 1, and the
        // only match sits at offset 0, behind the scan start.
        cx.update(|window, cx| bar.update(cx, |bar, cx| bar.next_match(window, cx)));
        assert_eq!(selection(&bar, cx), Some(Selection { anchor: ByteOffset::new(0), cursor: ByteOffset::new(1) }));
        assert_eq!(cx.update(|window, cx| window.notifications(cx).len()), 1, "the first hit already wrapped");

        // Second Next has nowhere to go but wrap back to the same match.
        cx.update(|window, cx| bar.update(cx, |bar, cx| bar.next_match(window, cx)));
        assert_eq!(selection(&bar, cx), Some(Selection { anchor: ByteOffset::new(0), cursor: ByteOffset::new(1) }));
        let effects = bar.read_with(cx, |bar, _| bar.state.pending_effects.clone());
        assert!(effects.contains(&SearchSideEffect::WrappedForward), "wrap must be queued");
        assert_eq!(cx.update(|window, cx| window.notifications(cx).len()), 2, "each wrap surfaces its own toast");
    }

    /// Replace-current overwrites the matched bytes in place when the
    /// find/replace lengths are equal (no splice-prompt dialog needed).
    #[gpui::test]
    fn replace_current_rewrites_matched_bytes(cx: &mut TestAppContext) {
        setup(cx);
        let bytes = vec![0xDEu8, 0xAD, 0xBE, 0xEF];
        let (bar, cx) = build(cx, bytes);

        bar.update(cx, |bar, _| bar.state.kind = SearchKind::HexBytes);
        set_query(&bar, "DE AD", cx);
        cx.update(|window, cx| bar.update(cx, |bar, cx| bar.next_match(window, cx)));
        set_replace(&bar, "CA FE", cx);

        cx.update(|window, cx| bar.update(cx, |bar, cx| bar.queue_replace_current(window, cx)));
        cx.run_until_parked();

        let bytes_after = read_bytes(&bar, ByteRange::new(ByteOffset::new(0), ByteOffset::new(4)).unwrap(), cx);
        assert_eq!(bytes_after, vec![0xCA, 0xFE, 0xBE, 0xEF]);
        // The caret advances past the replacement, matching egui's
        // `perform_replace_current`.
        assert_eq!(selection(&bar, cx), Some(Selection::caret(ByteOffset::new(2))));
        // 2, not 1: the positioning `next_match` above also wraps (the
        // only match sits at offset 0, behind its offset-1 scan start)
        // and toasts, same as `next_wraps_past_eof_and_toasts`; the
        // replace itself adds a second, success toast.
        let count = cx.update(|window, cx| window.notifications(cx).len());
        assert_eq!(count, 2, "wrap + a completed replace must both toast");
    }

    /// A find/replace pair of different lengths routes replace-current
    /// through the length-mismatch confirm dialog: nothing splices
    /// until it's confirmed, and confirming performs the resize.
    #[gpui::test]
    fn replace_current_with_length_mismatch_confirms_before_splicing(cx: &mut TestAppContext) {
        setup(cx);
        let bytes = vec![0xDEu8, 0xAD, 0xBE, 0xEF];
        let (bar, cx) = build(cx, bytes.clone());

        bar.update(cx, |bar, _| bar.state.kind = SearchKind::HexBytes);
        set_query(&bar, "DE AD", cx);
        cx.update(|window, cx| bar.update(cx, |bar, cx| bar.next_match(window, cx)));
        set_replace(&bar, "CA FE 01", cx);

        cx.update(|window, cx| bar.update(cx, |bar, cx| bar.queue_replace_current(window, cx)));
        cx.run_until_parked();

        let before = ByteRange::new(ByteOffset::new(0), ByteOffset::new(4)).unwrap();
        assert_eq!(read_bytes(&bar, before, cx), bytes, "must wait for the length-mismatch confirm before splicing");

        // Confirm the real dialog via its own Enter-bound Confirm action
        // (same mechanism the replace-all count-confirm test drives).
        cx.simulate_keystrokes("enter");

        let new_len = bar.read_with(cx, |bar, cx| bar.pane().read(cx).editor().source().len().get());
        assert_eq!(new_len, 5, "the file grows by the length delta once confirmed");
        let after = ByteRange::new(ByteOffset::new(0), ByteOffset::new(5)).unwrap();
        assert_eq!(read_bytes(&bar, after, cx), vec![0xCA, 0xFE, 0x01, 0xBE, 0xEF]);
    }

    /// Replace-all applies every match as a single batched splice, so
    /// undoing once reverts the whole operation.
    #[gpui::test]
    fn replace_all_is_a_single_undo_entry(cx: &mut TestAppContext) {
        setup(cx);
        let bytes = vec![0xDEu8, 0xAD, 0u8, 0xDE, 0xAD, 0u8];
        let (bar, cx) = build(cx, bytes.clone());

        bar.update(cx, |bar, _| bar.state.kind = SearchKind::HexBytes);
        set_query(&bar, "DE AD", cx);
        set_replace(&bar, "CA FE", cx);

        // Two matches, so Replace All routes through the count-confirm
        // dialog rather than performing inline.
        cx.update(|window, cx| bar.update(cx, |bar, cx| bar.queue_replace_all(window, cx)));
        cx.run_until_parked();
        let whole = ByteRange::new(ByteOffset::new(0), ByteOffset::new(6)).unwrap();
        assert_eq!(read_bytes(&bar, whole, cx), bytes, "must wait for the confirm dialog before splicing");

        // Confirm the real dialog: opening it moved focus there and
        // bound Enter to gpui-component's own Confirm action, which
        // runs the `on_ok` closure this test is actually meant to
        // exercise (not just the splice helper it calls).
        cx.simulate_keystrokes("enter");

        let bytes_after = read_bytes(&bar, whole, cx);
        assert_eq!(bytes_after, vec![0xCA, 0xFE, 0u8, 0xCA, 0xFE, 0u8]);
        let (undo_len, can_undo) = bar.read_with(cx, |bar, cx| {
            let pane = bar.pane().read(cx);
            (pane.editor().undo_stack().len(), pane.editor().can_undo())
        });
        assert_eq!(undo_len, 1, "replace-all must be a single undo entry");
        assert!(can_undo);

        bar.update(cx, |bar, cx| {
            let pane = bar.pane().clone();
            pane.update(cx, |pane, cx| {
                pane.editor_mut().undo();
                cx.notify();
            });
        });
        assert_eq!(read_bytes(&bar, whole, cx), vec![0xDE, 0xAD, 0u8, 0xDE, 0xAD, 0u8], "one undo must revert the whole batch");
    }
}
