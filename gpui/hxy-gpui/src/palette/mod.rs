//! The command palette: a custom top-center overlay over
//! [`palette_core`], driven by the workspace's `cmd-shift-p` binding.
//!
//! Not the gpui-component `Dialog`: the palette needs top-center
//! anchoring, a dimmed click-to-close backdrop, and per-keystroke
//! filtering, none of which the dialog gives. The overlay renders as a
//! child of [`Workspace`](crate::workspace::Workspace); its input takes
//! focus on open and restores the previously-focused element on close,
//! so editor keys never leak to the grid while it is up.
//!
//! The mode cascade, entry vocabulary, and argument parsing live in
//! [`modes`] (framework-agnostic, unit-tested); action dispatch into
//! the workspace lives in [`apply`].

pub mod apply;
pub mod modes;

use std::borrow::Cow;

use gpui::App;
use gpui::AppContext;
use gpui::Context;
use gpui::Entity;
use gpui::FocusHandle;
use gpui::Focusable;
use gpui::Hsla;
use gpui::InteractiveElement;
use gpui::IntoElement;
use gpui::MouseButton;
use gpui::ParentElement;
use gpui::ScrollHandle;
use gpui::StatefulInteractiveElement;
use gpui::Styled;
use gpui::Subscription;
use gpui::WeakEntity;
use gpui::Window;
use gpui::div;
use gpui::px;
use gpui::component::ActiveTheme;
use gpui::component::Icon;
use gpui::component::IconName;
use gpui::component::Sizable;
use gpui::component::h_flex;
use gpui::component::input::Input;
use gpui::component::input::InputEvent;
use gpui::component::input::InputState;
use gpui::component::v_flex;
use palette_core::CaseMatching;
use palette_core::Entry;
use palette_core::MatchResult;
use palette_core::MatcherConfig;
use palette_core::Normalization;
use palette_core::State;
use palette_core::filter_and_sort;

use crate::assets::HxyIcon;
use crate::palette::modes::CompareSide;
use crate::palette::modes::PaletteAction;
use crate::palette::modes::PaletteContext;
use crate::palette::modes::PaletteMode;
use crate::palette::modes::PluginCascadeState;
use crate::palette::modes::PluginPromptState;
use crate::palette::modes::Shortcuts;
use crate::palette::modes::build_entries;
use crate::palette::modes::build_plugin_cascade_entries;
use crate::palette::modes::build_plugin_main_entries;
use crate::palette::modes::build_plugin_prompt_entry;
use crate::palette::modes::build_templates_mode_entries;
use crate::palette::modes::build_uninstall_entries;
use crate::menu::Redo;
use crate::menu::ToggleEditMode;
use crate::menu::Undo;
use crate::plugins::PluginHandlersGlobal;
use crate::templates::TemplateLibraryGlobal;
use crate::workspace::OpenFile;
use crate::workspace::OpenSettings;
use crate::workspace::ReopenClosedTab;
use crate::workspace::ToggleGlobalSearch;
use crate::workspace::ToggleInspector;
use crate::workspace::ToggleVim;
use crate::workspace::Workspace;
use hxy_vfs::VfsHandler;

gpui::actions!(hxy_gpui_palette, [PaletteUp, PaletteDown, PaletteDismiss]);

/// Vertical gap from the window top to the palette panel, matching the
/// egui palette's `TopCenter { y_offset: 72.0 }`.
const TOP_OFFSET: f32 = 72.0;
const PANEL_WIDTH: f32 = 560.0;
const LIST_MAX_HEIGHT: f32 = 360.0;

pub struct Palette {
    /// Back-reference for reading the active-file context and
    /// dispatching picks. Weak so the palette never keeps the workspace
    /// alive.
    workspace: WeakEntity<Workspace>,
    state: State,
    mode: PaletteMode,
    input: Entity<InputState>,
    _input_sub: Subscription,
    /// Focused element to restore when the palette closes (the grid, or
    /// the workspace handle when no file is open). Stashed on open.
    restore_focus: Option<FocusHandle>,
    /// The A side chosen during the compare cascade (`path`, whether it
    /// came from an open file), carried from the `CompareSideA` pick to
    /// the `CompareSideB` pick that spawns the tab. Cleared on close and
    /// whenever the cascade leaves the B step.
    compare_a: Option<(std::path::PathBuf, bool)>,
    /// The plugin sub-menu backing [`PaletteMode::PluginCascade`]. Set by
    /// [`enter_plugin_cascade`](Self::enter_plugin_cascade), cleared on any
    /// other mode transition and on close.
    plugin_cascade: Option<PluginCascadeState>,
    /// The pending question backing [`PaletteMode::PluginPrompt`]. Set by
    /// [`enter_plugin_prompt`](Self::enter_plugin_prompt), cleared like
    /// `plugin_cascade`.
    plugin_prompt: Option<PluginPromptState>,
    /// Scroll handle for the result list; drives scroll-into-view so the
    /// selected row stays visible under keyboard nav (egui's
    /// `scroll_to_selection`).
    scroll: ScrollHandle,
}

impl Palette {
    pub fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(hxy_i18n::t("gpui-palette-search-placeholder")));
        let input_sub = cx.subscribe_in(&input, window, Self::on_input_event);
        Self {
            workspace,
            state: State::default(),
            mode: PaletteMode::Main,
            input,
            _input_sub: input_sub,
            restore_focus: None,
            compare_a: None,
            plugin_cascade: None,
            plugin_prompt: None,
            scroll: ScrollHandle::new(),
        }
    }

    /// Reopen the palette straight into the compare B pick with A already
    /// chosen. Used by the workspace after a "browse" file dialog resolves
    /// the A side outside the overlay.
    pub(crate) fn open_compare_b_with_a(
        &mut self,
        a: (std::path::PathBuf, bool),
        restore: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.state.open {
            self.restore_focus = restore;
        }
        self.compare_a = Some(a);
        self.enter_mode(PaletteMode::CompareSideB, window, cx);
    }

    /// Open (or switch) the palette into the plugin sub-menu a completed
    /// invoke produced (egui `enter_plugin_cascade`). Reached from the
    /// workspace's outcome dispatch, so `restore` is only stashed when the
    /// palette was closed at the time (a fresh open); an already-open
    /// palette keeps the focus it captured on its first open.
    pub(crate) fn enter_plugin_cascade(
        &mut self,
        plugin_name: String,
        commands: Vec<hxy_plugin_host::PluginCommand>,
        restore: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.state.open {
            self.restore_focus = restore;
        }
        // `enter_mode` clears both plugin buffers, so set the cascade
        // after it (mirrors egui, where `enter_plugin_cascade` bypasses
        // the state-clearing `open_at`).
        self.enter_mode(PaletteMode::PluginCascade, window, cx);
        self.plugin_cascade = Some(PluginCascadeState { plugin_name, commands });
        cx.notify();
    }

    /// Open (or switch) the palette into an argument-style prompt for a
    /// plugin's pending question (egui `enter_plugin_prompt`). The plugin's
    /// `title` becomes the input hint and `default_value` pre-fills the
    /// answer; submitting routes back through `respond_to_prompt` on the
    /// same `(plugin_name, command_id)`.
    pub(crate) fn enter_plugin_prompt(
        &mut self,
        prompt: PluginPromptState,
        default_value: Option<String>,
        restore: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.state.open {
            self.restore_focus = restore;
        }
        self.enter_mode(PaletteMode::PluginPrompt, window, cx);
        // The plugin's question is the real hint; prefill any default so an
        // "edit existing value" flow starts from it.
        let title = prompt.title.clone();
        let prefill = default_value.unwrap_or_default();
        self.plugin_prompt = Some(prompt);
        self.state.query = prefill.clone();
        self.input.update(cx, |input, cx| {
            input.set_placeholder(title, window, cx);
            input.set_value(prefill, window, cx);
        });
        cx.notify();
    }

    /// Close the palette from outside the overlay (the workspace's `Done`
    /// outcome dispatch). Idempotent -- a plugin command picked from the
    /// Main list already closed the palette before its op ran.
    pub(crate) fn close_external(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close(window, cx);
    }

    pub(crate) fn is_open(&self) -> bool {
        self.state.open
    }

    #[cfg(test)]
    pub(crate) fn mode(&self) -> PaletteMode {
        self.mode
    }

    #[cfg(test)]
    pub(crate) fn query(&self) -> String {
        self.state.query.clone()
    }

    /// The `(plugin, command labels)` of the active cascade, for tests.
    #[cfg(test)]
    pub(crate) fn plugin_cascade_labels(&self) -> Option<(String, Vec<String>)> {
        self.plugin_cascade
            .as_ref()
            .map(|c| (c.plugin_name.clone(), c.commands.iter().map(|cmd| cmd.label.clone()).collect()))
    }

    /// The `(plugin, command id, title)` of the active prompt, for tests.
    #[cfg(test)]
    pub(crate) fn plugin_prompt_state(&self) -> Option<(String, String, String)> {
        self.plugin_prompt.as_ref().map(|p| (p.plugin_name.clone(), p.command_id.clone(), p.title.clone()))
    }

    #[cfg(test)]
    pub(crate) fn selected(&self) -> usize {
        self.state.selected
    }

    /// Set the query text through the real input (driving the same
    /// `Change` subscription production does), for tests.
    #[cfg(test)]
    pub(crate) fn set_query_for_test(&self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            input.set_value(text.to_string(), window, cx);
            // 0.5.2 `set_value` is silent (suppresses events); emit the
            // `Change` a real edit would so the palette re-filters.
            cx.emit(InputEvent::Change);
        });
    }

    /// Open the palette at `mode`, stashing `restore` as the focus to
    /// return to on close. A second open at `Main` while already open in
    /// `Main` toggles closed (matching the egui app's Cmd+Shift+P).
    pub fn toggle(&mut self, restore: Option<FocusHandle>, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.open {
            // Already open: re-invoking at Main closes; from a sub-mode it
            // returns to Main. Never re-stash `restore` here -- the input
            // holds focus now, so `restore` would point at the palette
            // itself and the grid would never get focus back on close.
            if self.mode == PaletteMode::Main {
                self.close(window, cx);
            } else {
                self.enter_mode(PaletteMode::Main, window, cx);
            }
        } else {
            self.restore_focus = restore;
            self.enter_mode(PaletteMode::Main, window, cx);
        }
    }

    /// Open (or re-target) the palette directly in QuickOpen tab-switch
    /// mode. A re-invoke while already in QuickOpen toggles closed, so the
    /// same chord opens and dismisses (egui's Cmd+P behaviour). `restore`
    /// is only stashed on a fresh open, never when already open.
    pub fn open_at_quick_open(&mut self, restore: Option<FocusHandle>, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.open && self.mode == PaletteMode::QuickOpen {
            self.close(window, cx);
            return;
        }
        if !self.state.open {
            self.restore_focus = restore;
        }
        self.enter_mode(PaletteMode::QuickOpen, window, cx);
    }

    /// Switch to `mode` (fresh query / selection / focus), keeping the
    /// palette open and the stashed restore-focus intact. Drives both
    /// the `SwitchMode` pick and the Escape cascade pop.
    fn enter_mode(&mut self, mode: PaletteMode, window: &mut Window, cx: &mut Context<Self>) {
        // The A pick only lives across the CompareSideA -> CompareSideB
        // hop; any other transition (back to Main, restarting at A)
        // drops it so a later compare never reuses a stale A.
        if mode != PaletteMode::CompareSideB {
            self.compare_a = None;
        }
        // Plugin buffers only live inside their own mode; any transition
        // drops them. `enter_plugin_cascade` / `enter_plugin_prompt` set
        // theirs after calling this (egui `open_at` parity).
        self.plugin_cascade = None;
        self.plugin_prompt = None;
        self.mode = mode;
        self.state.open();
        let placeholder = hxy_i18n::t(mode.hint_key());
        self.input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.set_placeholder(placeholder, window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.state.close();
        self.compare_a = None;
        self.plugin_cascade = None;
        self.plugin_prompt = None;
        if let Some(handle) = self.restore_focus.take() {
            window.focus(&handle, cx);
        }
        cx.notify();
    }

    /// Escape: pop one cascade level when the user's
    /// `palette_escape_pops_to_parent` setting is on (the default),
    /// else close outright from any mode (egui parity). Backdrop
    /// clicks always close regardless -- see the setting's doc.
    fn on_dismiss(&mut self, _: &PaletteDismiss, window: &mut Window, cx: &mut Context<Self>) {
        let pops = crate::settings::settings(cx).palette_escape_pops_to_parent;
        match self.mode.parent() {
            Some(parent) if pops => self.enter_mode(parent, window, cx),
            _ => self.close(window, cx),
        }
    }

    fn on_up(&mut self, _: &PaletteUp, window: &mut Window, cx: &mut Context<Self>) {
        let len = self.build(window, cx).1.len();
        if len == 0 {
            return;
        }
        self.state.selected = (self.state.selected + len - 1) % len;
        self.scroll.scroll_to_item(self.state.selected);
        cx.notify();
    }

    fn on_down(&mut self, _: &PaletteDown, window: &mut Window, cx: &mut Context<Self>) {
        let len = self.build(window, cx).1.len();
        if len == 0 {
            return;
        }
        self.state.selected = (self.state.selected + 1) % len;
        self.scroll.scroll_to_item(self.state.selected);
        cx.notify();
    }

    fn on_input_event(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => {
                self.state.query = self.input.read(cx).value().to_string();
                cx.notify();
            }
            InputEvent::PressEnter { .. } => self.pick_selected(window, cx),
            InputEvent::Focus | InputEvent::Blur => {}
        }
    }

    /// The keybinding hints for the Main list, resolved through the same
    /// bindings the workspace registered (so the palette advertises the
    /// live shortcut). ASCII (`cmd-o`) via [`unparse`](gpui::Keystroke::unparse).
    fn shortcuts(&self, window: &Window) -> Shortcuts {
        Shortcuts {
            open_file: shortcut_for(window, &OpenFile),
            toggle_vim: shortcut_for(window, &ToggleVim),
            toggle_inspector: shortcut_for(window, &ToggleInspector),
            toggle_global_search: shortcut_for(window, &ToggleGlobalSearch),
            open_settings: shortcut_for(window, &OpenSettings),
            undo: shortcut_for(window, &Undo),
            redo: shortcut_for(window, &Redo),
            toggle_edit_mode: shortcut_for(window, &ToggleEditMode),
            reopen_closed_tab: shortcut_for(window, &ReopenClosedTab),
        }
    }

    /// Build the current entry list and its filtered order. Argument
    /// modes and the `@` / `=` prefixes bypass fuzzy filtering; Main
    /// filters by title against the query.
    fn build(&self, window: &Window, cx: &App) -> (Vec<Entry<PaletteAction>>, Vec<MatchResult>) {
        let ctx = self.context(cx);
        let mut entries = match self.mode {
            PaletteMode::CompareSideA => self.build_compare_entries(CompareSide::A, cx),
            PaletteMode::CompareSideB => self.build_compare_entries(CompareSide::B, cx),
            PaletteMode::Templates => self.build_template_entries(TemplateScope::WholeFile, cx),
            PaletteMode::TemplatesAtSelection => self.build_template_entries(TemplateScope::Selection, cx),
            PaletteMode::UninstallTemplate => build_uninstall_mode_entries(),
            PaletteMode::PluginCascade => self.plugin_cascade_rows(),
            PaletteMode::PluginPrompt => self.plugin_prompt_rows(),
            PaletteMode::QuickOpen => self.build_tab_entries(cx),
            PaletteMode::Recent => self.build_recent_entries(cx),
            _ => build_entries(self.mode, &self.state.query, ctx, &self.shortcuts(window)),
        };
        // Loaded plugins append their commands to the Main list (egui
        // entries.rs:1081). Skipped under the `@` / `=` calculator
        // prefixes, which replace the whole list with a single row.
        if self.mode == PaletteMode::Main && !self.mode.bypasses_filter(&self.state.query) {
            entries.extend(self.plugin_main_rows(cx));
        }
        let filtered = if self.mode.bypasses_filter(&self.state.query) {
            (0..entries.len()).map(|index| MatchResult { index, match_indices: Vec::new() }).collect()
        } else {
            filter_and_sort(
                &self.state.query,
                &entries,
                &MatcherConfig::DEFAULT,
                CaseMatching::Smart,
                Normalization::Smart,
                entry_haystack,
            )
        };
        (entries, filtered)
    }

    fn context(&self, cx: &App) -> PaletteContext {
        self.workspace.upgrade().map(|ws| ws.read(cx).palette_context(cx)).unwrap_or_default()
    }

    /// The compare-pick rows for `side`: one per open file (picked from
    /// its live in-memory buffer) plus a disk "browse" row. Same file on
    /// both sides is allowed -- the B list is not filtered against the
    /// chosen A (mirrors the egui picker modal, `compare/picker.rs`).
    fn build_compare_entries(&self, side: CompareSide, cx: &App) -> Vec<Entry<PaletteAction>> {
        let mut out = Vec::new();
        let choices = self.workspace.upgrade().map(|ws| ws.read(cx).open_compare_choices(cx)).unwrap_or_default();
        for (name, path) in choices {
            let mut entry =
                Entry::new(name, PaletteAction::CompareSelectSource { side, path: path.clone(), from_open_file: true });
            if let Some(parent) = path.parent() {
                entry = entry.with_subtitle(parent.display().to_string());
            }
            out.push(entry);
        }
        out.push(Entry::new(hxy_i18n::t("compare-picker-browse"), PaletteAction::CompareBrowse(side)));
        out
    }

    /// The Run-Template rows for the current cascade: library entries
    /// ranked against the active file, bound to the whole file or to
    /// the live selection. An at-selection cascade whose selection
    /// vanished renders empty rather than degrading to whole-file
    /// runs (mirrors the egui Templates mode).
    fn build_template_entries(&self, scope: TemplateScope, cx: &App) -> Vec<Entry<PaletteAction>> {
        let range = match scope {
            TemplateScope::WholeFile => None,
            TemplateScope::Selection => {
                let selection = self.context(cx).selection.and_then(|(start, end)| {
                    hxy_core::ByteRange::new(hxy_core::ByteOffset::new(start), hxy_core::ByteOffset::new(end)).ok()
                });
                match selection {
                    Some(range) => Some(range),
                    None => return Vec::new(),
                }
            }
        };
        let Some(ws) = self.workspace.upgrade() else { return Vec::new() };
        let (extension, head_bytes) = ws.read(cx).template_palette_seed(cx);
        // Installed at startup by `main`; only a harness that never
        // set it lands here, and an empty list is the right render.
        let Some(library) = cx.try_global::<TemplateLibraryGlobal>() else { return Vec::new() };
        build_templates_mode_entries(&library.0, extension.as_deref(), &head_bytes, range)
    }

    /// Main-list rows for every loaded plugin's advertised commands.
    /// Reads the live [`PluginHandlersGlobal`]; `list_commands` returns
    /// empty for a plugin without the `commands` grant, so an ungranted
    /// plugin adds nothing.
    fn plugin_main_rows(&self, cx: &App) -> Vec<Entry<PaletteAction>> {
        let Some(handlers) = cx.try_global::<PluginHandlersGlobal>() else { return Vec::new() };
        let grouped: Vec<(String, Vec<hxy_plugin_host::PluginCommand>)> =
            handlers.0.iter().map(|h| (h.name().to_owned(), h.list_commands())).collect();
        build_plugin_main_entries(&grouped)
    }

    /// Rows for the active plugin cascade, or empty when none is set (the
    /// mode is only entered with a populated buffer, so empty is inert).
    fn plugin_cascade_rows(&self) -> Vec<Entry<PaletteAction>> {
        self.plugin_cascade
            .as_ref()
            .map(|c| build_plugin_cascade_entries(&c.plugin_name, &c.commands))
            .unwrap_or_default()
    }

    /// The single answer row for the active plugin prompt, baking the
    /// current query as the answer; empty when no prompt is set.
    fn plugin_prompt_rows(&self) -> Vec<Entry<PaletteAction>> {
        self.plugin_prompt.as_ref().map(|p| build_plugin_prompt_entry(p, &self.state.query)).unwrap_or_default()
    }

    /// QuickOpen rows: one per open tab across every dock region, fuzzy
    /// filtered by tab name (egui's `Mode::QuickOpen`). Empty when the
    /// workspace is gone.
    fn build_tab_entries(&self, cx: &App) -> Vec<Entry<PaletteAction>> {
        let Some(ws) = self.workspace.upgrade() else { return Vec::new() };
        ws.read(cx)
            .open_tab_labels(cx)
            .into_iter()
            .map(|(label, id)| Entry::new(label, PaletteAction::FocusTab(id)))
            .collect()
    }

    /// The recently-opened files as palette rows (egui's `Mode::Recent`),
    /// newest first. Label is the file name, subtitle the parent dir; a
    /// pick reopens the file through [`Workspace::open_path`]. Shares the
    /// Welcome panel's [`recent_rows`](crate::panels::recent_rows)
    /// name/path derivation.
    fn build_recent_entries(&self, cx: &App) -> Vec<Entry<PaletteAction>> {
        let recents = crate::settings::settings(cx).recent_files;
        crate::panels::recent_rows(&recents)
            .into_iter()
            .map(|(label, path)| {
                let mut entry = Entry::new(label, PaletteAction::OpenRecent(path.clone()));
                if let Some(parent) = path.parent() {
                    entry = entry.with_subtitle(parent.display().to_string());
                }
                entry
            })
            .collect()
    }

    fn pick_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (entries, filtered) = self.build(window, cx);
        let Some(hit) = filtered.get(self.state.selected) else { return };
        let entry = &entries[hit.index];
        if entry.disabled {
            return;
        }
        let action = entry.data.clone();
        self.pick(action, window, cx);
    }

    fn pick_row(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.state.selected = row;
        self.pick_selected(window, cx);
    }

    fn pick(&mut self, action: PaletteAction, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            PaletteAction::SwitchMode(mode) => self.enter_mode(mode, window, cx),
            PaletteAction::NoOp => self.close(window, cx),
            // Picking the A side advances the cascade to the B pick
            // without leaving the overlay.
            PaletteAction::CompareSelectSource { side: CompareSide::A, path, from_open_file } => {
                self.compare_a = Some((path, from_open_file));
                self.enter_mode(PaletteMode::CompareSideB, window, cx);
            }
            // Picking the B side completes the pair: close, then spawn.
            PaletteAction::CompareSelectSource { side: CompareSide::B, path, from_open_file } => {
                let a = self.compare_a.take();
                self.close(window, cx);
                if let (Some((a_path, a_open)), Some(ws)) = (a, self.workspace.upgrade()) {
                    ws.update(cx, |ws, cx| ws.open_compare(a_path, a_open, path, from_open_file, window, cx));
                }
            }
            // Browse opens a disk file dialog; the workspace resolves the
            // async result and either advances to B (side A) or spawns
            // the compare (side B).
            PaletteAction::CompareBrowse(side) => {
                let prior_a = self.compare_a.take();
                let restore = self.restore_focus.clone();
                self.close(window, cx);
                if let Some(ws) = self.workspace.upgrade() {
                    ws.update(cx, |ws, cx| ws.compare_browse(side, prior_a, restore, window, cx));
                }
            }
            // QuickOpen pick: close, then bring the chosen tab forward and
            // focus it (activate_tab focuses the panel itself, so the
            // stashed restore-focus is irrelevant here).
            PaletteAction::FocusTab(id) => {
                self.close(window, cx);
                if let Some(ws) = self.workspace.upgrade() {
                    ws.update(cx, |ws, cx| ws.activate_tab(id, window, cx));
                }
            }
            // Close first (restoring focus to the grid), then act, so
            // the caret / clipboard change lands on what the user sees.
            other => {
                self.close(window, cx);
                if let Some(ws) = self.workspace.upgrade() {
                    ws.update(cx, |ws, cx| apply::apply(ws, other, window, cx));
                }
            }
        }
    }
}

/// Which byte scope a template cascade binds its picks to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TemplateScope {
    WholeFile,
    Selection,
}

/// Rows for the uninstall cascade: the template sources currently on
/// disk in the user templates directory. An unresolvable data dir
/// means nothing is installed there, so the list renders empty.
fn build_uninstall_mode_entries() -> Vec<Entry<PaletteAction>> {
    let installed = hxy_templates::user_templates_dir()
        .map(|dir| hxy_templates::library::list_installed_templates(&dir))
        .unwrap_or_default();
    build_uninstall_entries(&installed)
}

/// The highest-precedence binding for `action`, unparsed to an ASCII
/// chord (`cmd-shift-p`), or `None` when nothing is bound.
fn shortcut_for(window: &Window, action: &dyn gpui::Action) -> Option<String> {
    let binding = window.highest_precedence_binding_for_action(action)?;
    binding.keystrokes().first().map(|key| gpui::AsKeystroke::as_keystroke(key).unparse())
}

impl gpui::Render for Palette {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.state.open {
            return div().into_any_element();
        }

        let (entries, filtered) = self.build(window, cx);
        // Snap back to the best match on a query change; otherwise keep
        // selection in range (mirrors egui_palette::show). The snapshot
        // advance in `query_changed_since_last_frame` must run every
        // frame, so it stays the first operand of the `||`.
        let query_changed = self.state.query_changed_since_last_frame();
        if query_changed || filtered.is_empty() {
            self.state.selected = 0;
        } else {
            self.state.selected = self.state.selected.min(filtered.len() - 1);
        }
        // Keep the selection visible: keyboard nav scrolls from its own
        // handlers; a query change snaps the list back to the top.
        if query_changed {
            self.scroll.scroll_to_item(0);
        }

        // Copy every color out up front so no `&Theme` borrow lingers
        // across the row builders below.
        let (base, muted, hit_color, selected_bg, popover, border) = {
            let theme = cx.theme();
            (theme.foreground, theme.muted_foreground, theme.primary, theme.list_active, theme.popover, theme.border)
        };

        let list: gpui::AnyElement = if filtered.is_empty() {
            div().p_3().text_color(muted).child(hxy_i18n::t("gpui-palette-no-matches")).into_any_element()
        } else {
            let weak = cx.entity().downgrade();
            let selected = self.state.selected;
            let rows = filtered.iter().enumerate().map(|(row, hit)| {
                let entry = &entries[hit.index];
                Self::render_row(&weak, row, entry, &hit.match_indices, row == selected, base, muted, hit_color, selected_bg)
            });
            // Sizes to content up to LIST_MAX_HEIGHT, then scrolls; the scroll
            // handle keeps the selected row in view under keyboard nav. A
            // uniform_list is not usable here -- it does not size to content in
            // this floating popover and so rendered an empty list.
            v_flex()
                .id("palette-rows")
                .p_1()
                .gap_1()
                .max_h(px(LIST_MAX_HEIGHT))
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .children(rows)
                .into_any_element()
        };

        let backdrop = div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .bg(gpui::black().opacity(0.35))
            .occlude()
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _ev, window, cx| this.close(window, cx)));

        let panel = v_flex()
            .mt(px(TOP_OFFSET))
            .w(px(PANEL_WIDTH))
            .bg(popover)
            .border_1()
            .border_color(border)
            .rounded_lg()
            .shadow_lg()
            .occlude()
            .child(
                div()
                    .p_2()
                    .border_b_1()
                    .border_color(border)
                    .child(Input::new(&self.input).prefix(Icon::new(IconName::Search))),
            )
            .child(list);

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .key_context("Palette")
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_dismiss))
            .child(backdrop)
            .child(panel)
            .into_any_element()
    }
}

impl Palette {
    #[allow(clippy::too_many_arguments)]
    fn render_row(
        weak: &WeakEntity<Self>,
        row: usize,
        entry: &Entry<PaletteAction>,
        match_indices: &[u32],
        selected: bool,
        base: Hsla,
        muted: Hsla,
        hit_color: Hsla,
        selected_bg: Hsla,
    ) -> gpui::AnyElement {
        // Disabled rows dim uniformly and ignore clicks (the pick path
        // rejects them too); a stray click just re-selects the row.
        let title_color = if entry.disabled { muted } else { base };
        // The fuzzy haystack is `title + " " + subtitle` (see `entry_haystack`),
        // so split the match indices back at the title boundary: indices below
        // the title length highlight the title, the rest the subtitle.
        let title_len = entry.title.chars().count() as u32;
        let title_hits: Vec<u32> = match_indices.iter().copied().filter(|&index| index < title_len).collect();
        let title = highlighted_text(&entry.title, &title_hits, title_color, hit_color, entry.disabled);

        let mut left = h_flex().gap_2().items_center();
        // An explicit token (see `modes::ICON_WARNING`, `modes::ICON_PLUGIN`)
        // wins; otherwise the command's action picks the glyph, mirroring
        // egui's per-command icons (`entries.rs`).
        let icon = match entry.icon.as_deref() {
            Some(modes::ICON_WARNING) => Some(Icon::new(IconName::TriangleAlert)),
            Some(modes::ICON_PLUGIN) => Some(Icon::new(HxyIcon::PuzzlePiece)),
            _ => action_icon(&entry.data),
        };
        if let Some(icon) = icon {
            left = left.child(icon.small().text_color(title_color));
        }
        left = left.child(title);
        if let Some(subtitle) = &entry.subtitle {
            // Indices past the title and its one-space separator map into the
            // subtitle; rebase them to the subtitle's own character offsets.
            let sub_hits: Vec<u32> =
                match_indices.iter().copied().filter(|&index| index > title_len).map(|index| index - title_len - 1).collect();
            left = left.child(div().text_sm().child(highlighted_text(subtitle, &sub_hits, muted, hit_color, entry.disabled)));
        }

        let mut row_el =
            h_flex().w_full().items_center().justify_between().gap_2().px_2().py_1().rounded_md().child(left);
        if let Some(shortcut) = &entry.shortcut {
            row_el = row_el.child(div().text_color(muted).text_sm().child(shortcut.clone()));
        }
        if selected {
            row_el = row_el.bg(selected_bg);
        }
        let weak = weak.clone();
        row_el
            .on_mouse_down(MouseButton::Left, move |_ev, window, cx| {
                let _ = weak.update(cx, |this, cx| this.pick_row(row, window, cx));
            })
            .into_any_element()
    }
}

/// The leading icon for a command row, mirroring egui's per-command glyphs
/// (`commands/palette/entries.rs`). Commands with no fitting glyph in the
/// served icon set render icon-less rather than borrow a misleading one.
fn action_icon(action: &PaletteAction) -> Option<Icon> {
    use PaletteAction as A;
    let icon = match action {
        A::OpenFile => Icon::new(IconName::FolderOpen),
        A::OpenRecent(_) => Icon::new(IconName::File),
        A::CompareBrowse(_) => Icon::new(IconName::FolderOpen),
        A::BrowseVfs => Icon::new(HxyIcon::TreeStructure),
        A::CloseTab | A::TearTab => Icon::new(IconName::Close),
        A::OpenConsole => Icon::new(IconName::SquareTerminal),
        A::OpenMemory => Icon::new(IconName::ChartPie),
        A::ToggleInspector => Icon::new(IconName::Eye),
        A::ToggleGlobalSearch => Icon::new(IconName::Search),
        A::OpenStrings => Icon::new(IconName::CaseSensitive),
        A::OpenSettings => Icon::new(IconName::Settings),
        A::OpenPlugins => Icon::new(HxyIcon::PuzzlePiece),
        A::OpenVisualizer => Icon::new(IconName::LayoutDashboard),
        A::OpenEntropy => Icon::new(IconName::ChartPie),
        A::CopyText(_) | A::CopySelection(_) => Icon::new(IconName::Copy),
        A::RunTemplate { .. } | A::RunTemplateDialog | A::InstallTemplate => Icon::new(HxyIcon::Scroll),
        A::UninstallTemplate(_) => Icon::new(IconName::Delete),
        A::JumpNextField => Icon::new(IconName::ArrowRight),
        A::JumpPrevField => Icon::new(IconName::ArrowLeft),
        A::Undo => Icon::new(IconName::Undo),
        A::Redo => Icon::new(IconName::Redo),
        A::SetWatchMode(hxy_settings::AutoReloadMode::Never) => Icon::new(IconName::EyeOff),
        A::SetWatchMode(_) => Icon::new(IconName::Eye),
        A::FetchImhexPatterns => Icon::new(IconName::Globe),
        A::RespondToPlugin { .. } => Icon::new(HxyIcon::PuzzlePiece),
        _ => return None,
    };
    Some(icon)
}

/// Fuzzy haystack for an entry: title plus subtitle (space-joined) so a row
/// is findable by either -- egui matches both. `render_row` splits the match
/// indices back at the title boundary to highlight the title and subtitle.
fn entry_haystack<A>(entry: &Entry<A>) -> Cow<'_, str> {
    match &entry.subtitle {
        Some(subtitle) => Cow::Owned(format!("{} {subtitle}", entry.title)),
        None => Cow::Borrowed(entry.title.as_str()),
    }
}

/// Render `text` with the fuzzy-matched character positions painted in
/// `hit_color`, everything else in `base`. Empty `indices` renders one
/// flat run. Disabled rows pass `disabled = true` so even matched chars
/// stay muted.
fn highlighted_text(text: &str, indices: &[u32], base: Hsla, hit_color: Hsla, disabled: bool) -> impl IntoElement {
    let mut runs = h_flex();
    if indices.is_empty() || disabled {
        return runs.child(div().text_color(base).child(text.to_owned()));
    }
    let mut cursor = 0usize;
    let mut current = String::new();
    let mut current_hit = false;
    for (char_idx, ch) in text.chars().enumerate() {
        while cursor < indices.len() && (indices[cursor] as usize) < char_idx {
            cursor += 1;
        }
        let is_hit = cursor < indices.len() && indices[cursor] as usize == char_idx;
        if char_idx == 0 {
            current_hit = is_hit;
        } else if is_hit != current_hit {
            let color = if current_hit { hit_color } else { base };
            runs = runs.child(div().text_color(color).child(std::mem::take(&mut current)));
            current_hit = is_hit;
        }
        current.push(ch);
    }
    if !current.is_empty() {
        let color = if current_hit { hit_color } else { base };
        runs = runs.child(div().text_color(color).child(current));
    }
    runs
}

impl Focusable for Palette {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Arc;

    use gpui::TestAppContext;
    use gpui::VisualTestContext;
    use hxy_core::ByteOffset;
    use hxy_core::Selection;
    use hxy_plugin_host::InMemoryStateStore;
    use hxy_plugin_host::PermissionGrants;
    use hxy_plugin_host::PluginGrants;
    use hxy_plugin_host::PluginHandler;
    use hxy_plugin_host::PluginKey;
    use hxy_plugin_host::StateStore;

    use super::*;
    use crate::workspace::Workspace;

    fn setup(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui::component::init(cx);
            crate::panels::register(cx);
            crate::workspace::init_keybindings(cx);
        });
    }

    /// A workspace inside a real `gpui::component::Root` (like the shell),
    /// opened on a `len`-byte scratch file so the palette has an active
    /// pane to act on. The backing temp file is read at construction and
    /// dropped once `build` returns; its bytes already live in the
    /// panel's in-memory source by then.
    fn build(cx: &mut TestAppContext, len: usize) -> (Entity<Workspace>, &mut VisualTestContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.bin");
        std::fs::write(&path, vec![0u8; len]).unwrap();
        let window = cx.add_window(move |window, cx| {
            let sub = window.observe_window_appearance(|_, _| {});
            let ws = cx.new(|cx| Workspace::new(vec![path], sub, None, window, cx));
            gpui::component::Root::new(ws, window, cx)
        });
        let root = window.root(cx).unwrap();
        let ws = root.read_with(cx, |root, _| root.view().clone().downcast::<Workspace>().unwrap());
        let vcx = VisualTestContext::from_window(*window, cx).into_mut();
        vcx.run_until_parked();
        (ws, vcx)
    }

    fn palette(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<Palette> {
        ws.read_with(cx, |ws, _| ws.palette())
    }

    fn caret(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<u64> {
        ws.read_with(cx, |ws, cx| {
            ws.active_pane(cx).and_then(|p| p.read(cx).editor().selection()).map(|s| s.cursor.get())
        })
    }

    fn seed_caret(ws: &Entity<Workspace>, at: u64, cx: &mut VisualTestContext) {
        cx.update(|_window, cx| {
            ws.update(cx, |ws, cx| {
                if let Some(pane) = ws.active_pane(cx) {
                    pane.update(cx, |pane, cx| {
                        pane.editor_mut().set_selection(Some(Selection::caret(ByteOffset::new(at))));
                        cx.notify();
                    });
                }
            });
        });
        cx.run_until_parked();
    }

    fn type_query(pal: &Entity<Palette>, text: &str, cx: &mut VisualTestContext) {
        cx.update(|window, cx| pal.update(cx, |pal, cx| pal.set_query_for_test(text, window, cx)));
        cx.run_until_parked();
    }

    /// End-to-end: open the palette, cascade into Go-to-offset via a
    /// real fuzzy pick, type a relative `+10`, press Enter, and the
    /// caret jumps to cursor+10.
    #[gpui::test]
    fn go_to_offset_relative_jumps_the_caret(cx: &mut TestAppContext) {
        setup(cx);
        let (ws, cx) = build(cx, 32);
        seed_caret(&ws, 5, cx);
        let pal = palette(&ws, cx);

        cx.simulate_keystrokes("cmd-shift-p");
        assert!(pal.read_with(cx, |p, _| p.is_open()), "cmd-shift-p opens the palette");

        // Fuzzy-pick the Go-to-offset command, then supply the argument.
        type_query(&pal, "go to offset", cx);
        cx.simulate_keystrokes("enter");
        assert_eq!(pal.read_with(cx, |p, _| p.mode()), PaletteMode::GoToOffset);

        type_query(&pal, "+10", cx);
        cx.simulate_keystrokes("enter");

        assert_eq!(caret(&ws, cx), Some(15), "relative +10 from cursor 5 lands at 15");
        assert!(!pal.read_with(cx, |p, _| p.is_open()), "picking closes the palette");
    }

    /// Same flow as `go_to_offset_relative_jumps_the_caret`, but with
    /// the file's own strings tab focused instead of its own tab:
    /// `self.active_file` is `None` throughout (a strings tab has no
    /// `FilePanel` representation), so the palette entry staying
    /// enabled and the jump landing on the right file both depend on
    /// `palette_context` / `active_pane` routing through
    /// `Workspace::reference_active_file`'s fallback.
    #[gpui::test]
    fn go_to_offset_jumps_the_reference_file_while_its_strings_tab_is_focused(cx: &mut TestAppContext) {
        setup(cx);
        let (ws, cx) = build(cx, 32);
        seed_caret(&ws, 5, cx);

        cx.update(|window, cx| ws.update(cx, |ws, cx| ws.open_strings_for_active_file(window, cx)));
        cx.run_until_parked();
        assert!(!ws.read_with(cx, |ws, _| ws.has_strict_active_file()), "sanity: strings tab is front-most");

        let pal = palette(&ws, cx);
        cx.simulate_keystrokes("cmd-shift-p");
        assert!(pal.read_with(cx, |p, _| p.is_open()), "cmd-shift-p opens the palette");

        type_query(&pal, "go to offset", cx);
        cx.simulate_keystrokes("enter");
        assert_eq!(pal.read_with(cx, |p, _| p.mode()), PaletteMode::GoToOffset, "entry stayed enabled");

        type_query(&pal, "+10", cx);
        cx.simulate_keystrokes("enter");

        assert_eq!(caret(&ws, cx), Some(15), "jump landed on the reference file, not nowhere");
    }

    /// Escape pops one cascade level at a time: Go-to-offset -> Main,
    /// then Main -> closed.
    #[gpui::test]
    fn escape_pops_the_cascade_then_closes(cx: &mut TestAppContext) {
        setup(cx);
        let (ws, cx) = build(cx, 32);
        let pal = palette(&ws, cx);

        cx.simulate_keystrokes("cmd-shift-p");
        type_query(&pal, "go to offset", cx);
        cx.simulate_keystrokes("enter");
        assert_eq!(pal.read_with(cx, |p, _| p.mode()), PaletteMode::GoToOffset);

        cx.simulate_keystrokes("escape");
        assert!(pal.read_with(cx, |p, _| p.is_open()), "escape from a sub-mode keeps the palette open");
        assert_eq!(pal.read_with(cx, |p, _| p.mode()), PaletteMode::Main, "escape pops back to Main");

        cx.simulate_keystrokes("escape");
        assert!(!pal.read_with(cx, |p, _| p.is_open()), "escape at Main closes the palette");
    }

    /// With `palette_escape_pops_to_parent` off, Escape from a
    /// sub-mode closes the palette outright instead of popping.
    #[gpui::test]
    fn escape_closes_from_submode_when_pop_setting_is_off(cx: &mut TestAppContext) {
        setup(cx);
        cx.update(|cx| crate::settings::update_settings(cx, |s| s.palette_escape_pops_to_parent = false));
        let (ws, cx) = build(cx, 32);
        let pal = palette(&ws, cx);

        cx.simulate_keystrokes("cmd-shift-p");
        type_query(&pal, "go to offset", cx);
        cx.simulate_keystrokes("enter");
        assert_eq!(pal.read_with(cx, |p, _| p.mode()), PaletteMode::GoToOffset);

        cx.simulate_keystrokes("escape");
        assert!(!pal.read_with(cx, |p, _| p.is_open()), "escape closes outright with the setting off");
    }

    /// Down / Up move the selection through the list and wrap. Guards
    /// against the input's own arrow bindings swallowing palette nav.
    #[gpui::test]
    fn arrow_keys_move_the_selection(cx: &mut TestAppContext) {
        setup(cx);
        let (ws, cx) = build(cx, 32);
        let pal = palette(&ws, cx);

        cx.simulate_keystrokes("cmd-shift-p");
        assert_eq!(pal.read_with(cx, |p, _| p.selected()), 0);

        cx.simulate_keystrokes("down");
        assert_eq!(pal.read_with(cx, |p, _| p.selected()), 1, "down advances the selection");
        cx.simulate_keystrokes("down");
        assert_eq!(pal.read_with(cx, |p, _| p.selected()), 2);
        cx.simulate_keystrokes("up");
        assert_eq!(pal.read_with(cx, |p, _| p.selected()), 1, "up retreats the selection");
    }

    /// Re-invoking `cmd-shift-p` from a sub-mode returns to Main without
    /// losing the stashed grid focus, so a later close still restores the
    /// grid (regression for the re-stash-clobbers-restore-focus bug).
    #[gpui::test]
    fn reopen_from_submode_preserves_restore_focus(cx: &mut TestAppContext) {
        setup(cx);
        let (ws, cx) = build(cx, 32);
        let pal = palette(&ws, cx);
        let grid = ws.read_with(cx, |ws, cx| ws.active_pane(cx).unwrap().read(cx).focus_handle(cx));

        cx.simulate_keystrokes("cmd-shift-p");
        type_query(&pal, "go to offset", cx);
        cx.simulate_keystrokes("enter");
        assert_eq!(pal.read_with(cx, |p, _| p.mode()), PaletteMode::GoToOffset);

        // Re-open while in the sub-mode, then close: focus must land back
        // on the grid, not on the palette's own (now unrendered) input.
        cx.simulate_keystrokes("cmd-shift-p");
        assert_eq!(pal.read_with(cx, |p, _| p.mode()), PaletteMode::Main);
        cx.simulate_keystrokes("escape");
        assert!(!pal.read_with(cx, |p, _| p.is_open()));
        assert_eq!(cx.update(|window, cx| window.focused(cx)), Some(grid), "close restores grid focus");
    }

    /// Cmd+P opens QuickOpen and picking a background tab brings it to
    /// the front. Opens a strings tab (front-most, so the file tab is
    /// backgrounded), then fuzzy-picks the file tab by name: the file
    /// becomes the strict active tab again.
    #[gpui::test]
    fn quick_open_switches_to_a_background_tab(cx: &mut TestAppContext) {
        setup(cx);
        let (ws, cx) = build(cx, 32);

        cx.update(|window, cx| ws.update(cx, |ws, cx| ws.open_strings_for_active_file(window, cx)));
        cx.run_until_parked();
        assert!(!ws.read_with(cx, |ws, _| ws.has_strict_active_file()), "sanity: strings tab is front-most");

        let pal = palette(&ws, cx);
        cx.simulate_keystrokes("cmd-p");
        assert!(pal.read_with(cx, |p, _| p.is_open()), "cmd-p opens the palette");
        assert_eq!(pal.read_with(cx, |p, _| p.mode()), PaletteMode::QuickOpen, "cmd-p enters QuickOpen");

        type_query(&pal, "t.bin", cx);
        cx.simulate_keystrokes("enter");

        assert!(!pal.read_with(cx, |p, _| p.is_open()), "picking closes the palette");
        assert!(ws.read_with(cx, |ws, _| ws.has_strict_active_file()), "the file tab is front-most again");
    }

    /// Re-invoking Cmd+P while already in QuickOpen toggles the palette
    /// closed (egui's Cmd+P behaviour), mirroring `cmd-shift-p` at Main.
    #[gpui::test]
    fn quick_open_toggles_closed_on_reinvoke(cx: &mut TestAppContext) {
        setup(cx);
        let (ws, cx) = build(cx, 32);
        let pal = palette(&ws, cx);

        cx.simulate_keystrokes("cmd-p");
        assert!(pal.read_with(cx, |p, _| p.is_open()), "cmd-p opens QuickOpen");
        cx.simulate_keystrokes("cmd-p");
        assert!(!pal.read_with(cx, |p, _| p.is_open()), "re-invoking cmd-p closes it");
    }

    /// Clicking the dimmed backdrop fully closes the palette even from a
    /// sub-mode (unlike Escape, which pops one level) and restores focus
    /// to the grid.
    #[gpui::test]
    fn backdrop_click_closes_from_submode_and_restores_focus(cx: &mut TestAppContext) {
        setup(cx);
        let (ws, cx) = build(cx, 32);
        let pal = palette(&ws, cx);
        let grid = ws.read_with(cx, |ws, cx| ws.active_pane(cx).unwrap().read(cx).focus_handle(cx));

        cx.simulate_keystrokes("cmd-shift-p");
        type_query(&pal, "go to offset", cx);
        cx.simulate_keystrokes("enter");
        assert_eq!(pal.read_with(cx, |p, _| p.mode()), PaletteMode::GoToOffset);

        // Click the top-left corner: outside the top-center panel, so it
        // lands on the backdrop.
        cx.simulate_click(gpui::point(px(5.0), px(5.0)), gpui::Modifiers::default());
        assert!(!pal.read_with(cx, |p, _| p.is_open()), "backdrop click fully closes, even from a sub-mode");
        assert_eq!(cx.update(|window, cx| window.focused(cx)), Some(grid), "backdrop close restores grid focus");
    }

    /// The `=<expr>` calculator prefix copies the evaluated value: Enter
    /// on the top (decimal) row writes it to the clipboard.
    #[gpui::test]
    fn calculator_equals_copies_the_result(cx: &mut TestAppContext) {
        setup(cx);
        let (ws, cx) = build(cx, 32);
        let pal = palette(&ws, cx);

        cx.simulate_keystrokes("cmd-shift-p");
        type_query(&pal, "=2+2", cx);
        cx.simulate_keystrokes("enter");

        let clip = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(clip.as_deref(), Some("4"), "the decimal row copies the evaluated value");
        assert!(!pal.read_with(cx, |p, _| p.is_open()), "copying closes the palette");
    }

    /// End-to-end template cascade: pick "Run Template...", fuzzy-pick
    /// a library template ranked against the open file, and the run
    /// lands on the active file panel.
    #[gpui::test]
    fn templates_cascade_runs_a_library_template(cx: &mut TestAppContext) {
        setup(cx);
        let tpl_dir = tempfile::tempdir().unwrap();
        std::fs::write(tpl_dir.path().join("quad.bt"), "// File Mask: *.bin\nLittleEndian();\nuint32 a;\n").unwrap();
        cx.update(|cx| {
            cx.set_global(crate::templates::TemplateRuntimes(hxy_templates::builtin::builtins()));
            cx.set_global(TemplateLibraryGlobal(hxy_templates::library::TemplateLibrary::load_from(Some(
                tpl_dir.path(),
            ))));
        });
        let (ws, cx) = build(cx, 8);
        let pal = palette(&ws, cx);

        cx.simulate_keystrokes("cmd-shift-p");
        type_query(&pal, "run template", cx);
        cx.simulate_keystrokes("enter");
        assert_eq!(pal.read_with(cx, |p, _| p.mode()), PaletteMode::Templates, "cascaded into the template list");

        // The `.bin` file mask ranks quad.bt at the top; pick it.
        type_query(&pal, "quad", cx);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        assert!(!pal.read_with(cx, |p, _| p.is_open()), "picking closes the palette");
        let file = cx.update(|_window, cx| {
            cx.global::<crate::panels::strings::OpenFilePanels>().0.first().cloned().expect("one open file")
        });
        file.read_with(cx, |panel, _| {
            assert_eq!(panel.templates.len(), 1, "the pick ran the template");
            assert!(panel.templates[0].source_path.ends_with("quad.bt"));
            assert!(panel.templates[0].state.parsed.is_some(), "run completed successfully");
            assert!(panel.template_panel_visible);
        });
    }

    /// The prebuilt commands+state fixture component, or `None` when a
    /// fresh checkout has not built it yet (keeps `cargo test` green).
    fn statecmd_fixture() -> Option<PathBuf> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/test-statecmd/target/wasm32-wasip2/release/hxy_plugin_test_statecmd.wasm");
        path.exists().then_some(path)
    }

    /// Stage the fixture wasm + a permissive sidecar manifest, load a
    /// single granted handler, and install it as the live
    /// [`PluginHandlersGlobal`] so the palette lists its commands.
    fn install_fixture_plugin(cx: &mut VisualTestContext, fixture: &Path) {
        let bytes = std::fs::read(fixture).expect("read fixture");
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("test-statecmd.wasm"), &bytes).expect("stage wasm");
        std::fs::write(
            dir.path().join("test-statecmd.hxy.toml"),
            "[plugin]\nname = \"test-statecmd\"\nversion = \"0.1.0\"\n\n[permissions]\npersist = true\ncommands = true\n",
        )
        .expect("stage manifest");
        let mut grants = PluginGrants::default();
        let key = PluginKey::from_bytes("test-statecmd", "0.1.0", &bytes);
        grants.set(key, PermissionGrants { persist: true, commands: true, network: vec![] });
        let store: Arc<dyn StateStore> = Arc::new(InMemoryStateStore::new());
        let handlers: Vec<Arc<PluginHandler>> =
            hxy_plugin_host::load_plugins_from_dir(dir.path(), &grants, Some(store))
                .expect("load fixture plugin")
                .into_iter()
                .map(Arc::new)
                .collect();
        assert!(!handlers.is_empty(), "fixture handler loaded");
        // Keep the tempdir alive: the handler mmaps its component from disk.
        std::mem::forget(dir);
        cx.update(|_, cx| cx.set_global(PluginHandlersGlobal(handlers)));
    }

    /// A loaded plugin's Main-list commands appear (prefixed with the
    /// plugin name); picking a `Done` command closes the palette.
    #[gpui::test]
    fn plugin_command_lists_and_done_closes(cx: &mut TestAppContext) {
        let Some(fixture) = statecmd_fixture() else {
            eprintln!("skipping: test-statecmd fixture not built");
            return;
        };
        setup(cx);
        let (ws, cx) = build(cx, 32);
        install_fixture_plugin(cx, &fixture);
        let pal = palette(&ws, cx);

        cx.simulate_keystrokes("cmd-shift-p");
        type_query(&pal, "Done outcome", cx);
        // The plugin row is the only match for that query.
        assert_eq!(pal.read_with(cx, |p, _| p.mode()), PaletteMode::Main);

        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(!pal.read_with(cx, |p, _| p.is_open()), "a Done-outcome command closes the palette");
    }

    /// Invoking a command that returns `Cascade` re-enters the palette in
    /// `PluginCascade` mode listing the returned sub-commands.
    #[gpui::test]
    fn plugin_cascade_command_reenters_cascade_mode(cx: &mut TestAppContext) {
        let Some(fixture) = statecmd_fixture() else {
            eprintln!("skipping: test-statecmd fixture not built");
            return;
        };
        setup(cx);
        let (ws, cx) = build(cx, 32);
        install_fixture_plugin(cx, &fixture);
        let pal = palette(&ws, cx);

        cx.simulate_keystrokes("cmd-shift-p");
        type_query(&pal, "Cascade outcome", cx);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        assert!(pal.read_with(cx, |p, _| p.is_open()), "a Cascade outcome keeps the palette open");
        assert_eq!(pal.read_with(cx, |p, _| p.mode()), PaletteMode::PluginCascade);
        let (name, labels) = pal.read_with(cx, |p, _| p.plugin_cascade_labels()).expect("cascade state set");
        assert_eq!(name, "test-statecmd");
        assert_eq!(labels.len(), 2, "the two child commands populate the cascade");
        assert!(labels[0].starts_with("Child A"), "got {labels:?}");

        // Escape pops the cascade back to Main.
        cx.simulate_keystrokes("escape");
        assert_eq!(pal.read_with(cx, |p, _| p.mode()), PaletteMode::Main);
        assert!(pal.read_with(cx, |p, _| p.plugin_cascade_labels()).is_none(), "leaving the mode clears the buffer");
    }

    /// Invoking a command that returns `Prompt` re-enters the palette in
    /// `PluginPrompt` mode with the plugin's default answer pre-filled and
    /// its title carried for the reply routing.
    #[gpui::test]
    fn plugin_prompt_command_reenters_prompt_mode_prefilled(cx: &mut TestAppContext) {
        let Some(fixture) = statecmd_fixture() else {
            eprintln!("skipping: test-statecmd fixture not built");
            return;
        };
        setup(cx);
        let (ws, cx) = build(cx, 32);
        install_fixture_plugin(cx, &fixture);
        let pal = palette(&ws, cx);

        cx.simulate_keystrokes("cmd-shift-p");
        type_query(&pal, "Prompt outcome", cx);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        assert_eq!(pal.read_with(cx, |p, _| p.mode()), PaletteMode::PluginPrompt);
        let (name, command_id, title) = pal.read_with(cx, |p, _| p.plugin_prompt_state()).expect("prompt state set");
        assert_eq!(name, "test-statecmd");
        assert_eq!(command_id, "prompt", "the reply reuses the originating command id");
        assert_eq!(title, "Token name");
        assert!(pal.read_with(cx, |p, _| p.query()).starts_with("default-"), "the default value pre-fills the input");
    }

    /// End-to-end compare cascade: pick "Compare files...", choose the
    /// open file for A, then the same open file for B (same-file-both-
    /// sides is allowed), and a `ComparePanel` tab is spawned.
    #[gpui::test]
    fn compare_cascade_spawns_a_compare_tab(cx: &mut TestAppContext) {
        setup(cx);
        let (ws, cx) = build(cx, 32);
        let pal = palette(&ws, cx);

        cx.simulate_keystrokes("cmd-shift-p");
        type_query(&pal, "compare files", cx);
        cx.simulate_keystrokes("enter");
        assert_eq!(pal.read_with(cx, |p, _| p.mode()), PaletteMode::CompareSideA, "cascaded into the A pick");

        // Pick the one open file (t.bin) for A, advancing to the B pick.
        type_query(&pal, "t.bin", cx);
        cx.simulate_keystrokes("enter");
        assert_eq!(pal.read_with(cx, |p, _| p.mode()), PaletteMode::CompareSideB, "A picked, now on B");

        // Pick the same file for B: spawns the compare and closes.
        type_query(&pal, "t.bin", cx);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        assert!(!pal.read_with(cx, |p, _| p.is_open()), "picking B closes the palette");
        let names = ws.read_with(cx, |ws, cx| ws.center_panel_names(cx));
        assert!(
            names.iter().any(|n| n == crate::panels::COMPARE_PANEL_NAME),
            "a compare tab was spawned, got {names:?}"
        );
    }
}
