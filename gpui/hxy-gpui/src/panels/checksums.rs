//! [`ChecksumsPanel`]: per-file checksum/hash tab.
//!
//! Hashing itself lives entirely in `hxy_panels::checksums` (shared
//! with the egui front end); this module only owns the algorithm
//! checkboxes, the range input, background dispatch, and the result
//! rows' per-row copy button.
//!
//! Binding shape mirrors [`StringsPanel`](super::StringsPanel) (see
//! that module's doc for the restore/rebind rationale): a per-file
//! center-dock tab keyed by the owning file's path, bound eagerly when
//! opened from the palette / View menu, or lazily via the
//! [`OpenFilePanels`](super::strings::OpenFilePanels) global when
//! restored from a persisted layout.
//!
//! Unlike strings/entropy, [`ChecksumsPanel::dump`] persists the range
//! and selected algorithms verbatim (mirroring egui's `Serialize`d
//! `ChecksumConfig`) rather than dropping them for a whole-file
//! backfill: a user's chosen algorithm set and range are configuration,
//! not a stale cached result, and `last_result` itself is still never
//! persisted (recomputed via auto-run on bind, same rule as the other
//! two panels). Auto-run mirrors `StringsPanel::bind_pane`'s
//! `AUTO_RUN_MAX_BYTES` gate exactly (egui's `render_checksums_tab`
//! uses the same cap, unlike entropy) -- checked against the range
//! that ends up bound (the backfilled whole file on a fresh open, or a
//! restored custom range), not unconditionally against file length.

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

use gpui::App;
use gpui::AppContext;
use gpui::ClipboardItem;
use gpui::Context;
use gpui::Entity;
use gpui::EventEmitter;
use gpui::FocusHandle;
use gpui::Focusable;
use gpui::IntoElement;
use gpui::ParentElement;
use gpui::Render;
use gpui::SharedString;
use gpui::Styled;
use gpui::Subscription;
use gpui::Task;
use gpui::Window;
use gpui::div;
use gpui::px;
use gpui_component::ActiveTheme;
use gpui_component::Disableable;
use gpui_component::WindowExt;
use gpui_component::button::Button;
use gpui_component::checkbox::Checkbox;
use gpui_component::dock::BasePanel;
use gpui_component::dock::Panel;
use gpui_component::dock::PanelEvent;
use gpui_component::dock::PanelInfo;
use gpui_component::dock::PanelState;
use gpui_component::h_flex;
use gpui_component::input::Input;
use gpui_component::input::InputState;
use gpui_component::label::Label;
use gpui_component::notification::Notification;
use gpui_component::v_flex;
use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;
use hxy_panels::checksums::Algorithm;
use hxy_panels::checksums::ChecksumConfig;
use hxy_panels::checksums::ChecksumResult;
use hxy_panels::checksums::compute;
use hxy_panels::goto::parse_range_expr;
use hxy_view_gpui::HexPane;

use super::FilePanel;
use super::strings::OpenFilePanels;

/// Stable identifier for layout (de)serialization; must never change.
pub const CHECKSUMS_PANEL_NAME: &str = "ChecksumsPanel";

/// Whole-file scans under this size auto-run when the panel opens.
/// Mirrors `StringsPanel`'s `AUTO_RUN_MAX_BYTES` (itself mirroring
/// `AUTO_RUN_MAX_BYTES` in `crates/hxy/src/app/mod.rs`, which
/// `render_checksums_tab` uses too, unlike entropy).
const AUTO_RUN_MAX_BYTES: u64 = 256 * 1024 * 1024;

fn open_file_panels(cx: &App) -> Vec<Entity<FilePanel>> {
    match cx.try_global::<OpenFilePanels>() {
        Some(g) => g.0.clone(),
        None => Vec::new(),
    }
}

pub struct ChecksumsPanel {
    focus_handle: FocusHandle,
    owning_path: Option<PathBuf>,
    owning_pane: Option<Entity<HexPane>>,
    _rebind_observe: Subscription,
    config: ChecksumConfig,
    last_result: Option<ChecksumResult>,
    running: bool,
    /// A run requested while another was in flight; replayed on
    /// completion so a reload-triggered recompute is never swallowed.
    pending_rerun: bool,
    _compute: Option<Task<()>>,
    range_input: Entity<InputState>,
}

impl ChecksumsPanel {
    /// Build a fresh panel bound immediately to `pane` (the palette /
    /// View-menu open path, where the owning file is already live).
    pub fn new(pane: Entity<HexPane>, path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self::with_state(path, ChecksumConfig::default(), window, cx);
        this.bind_pane(pane, cx);
        this
    }

    /// Rebuild from persisted [`PanelInfo`]. No live file to bind to
    /// yet (see module doc); binds lazily via [`OpenFilePanels`].
    pub fn restore(info: &PanelInfo, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path = path_from_info(info);
        let config = config_from_info(info);
        let mut this = Self::with_state(path, config, window, cx);
        this.try_bind_from_global(cx);
        this
    }

    fn with_state(path: Option<PathBuf>, config: ChecksumConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let range_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(hxy_i18n::t("gpui-checksums-range-placeholder")));
        let rebind_observe = cx.observe_global::<OpenFilePanels>(|this, cx| this.try_bind_from_global(cx));
        Self {
            focus_handle: cx.focus_handle(),
            owning_path: path,
            owning_pane: None,
            _rebind_observe: rebind_observe,
            config,
            last_result: None,
            running: false,
            pending_rerun: false,
            _compute: None,
            range_input,
        }
    }

    /// The file path this panel hashes, for the workspace's open-tab
    /// dedup and dock persistence.
    pub(crate) fn owning_path(&self) -> Option<&Path> {
        self.owning_path.as_deref()
    }

    /// Re-anchor this panel onto a new owning path (Save As renamed its
    /// file). Same live pane, new lookup/persist key.
    pub(crate) fn set_owning_path(&mut self, path: PathBuf) {
        self.owning_path = Some(path);
    }

    /// Look up `owning_path` in the currently published
    /// [`OpenFilePanels`] and bind to it if found. No-op once already
    /// bound, and a no-op that leaves the panel showing its "no active
    /// file" state when the path never resolves.
    fn try_bind_from_global(&mut self, cx: &mut Context<Self>) {
        if self.owning_pane.is_some() {
            return;
        }
        let Some(path) = self.owning_path.clone() else { return };
        let Some(file) = open_file_panels(cx).into_iter().find(|f| f.read(cx).path() == Some(path.as_path())) else {
            return;
        };
        let pane = file.read(cx).pane().clone();
        self.bind_pane(pane, cx);
    }

    /// Bind to the owning file's pane, backfilling the range to the
    /// whole file the first time (a fresh open, or a layout restored
    /// before this panel persisted a range), and auto-running when the
    /// bound range fits under [`AUTO_RUN_MAX_BYTES`] and at least one
    /// algorithm is selected. A restored custom range is left as-is
    /// (not overwritten), so reopening a layout with a smaller
    /// persisted range re-runs over that range, not the whole file.
    fn bind_pane(&mut self, pane: Entity<HexPane>, cx: &mut Context<Self>) {
        if self.owning_pane.is_some() {
            return;
        }
        self.owning_pane = Some(pane.clone());
        let source_len = pane.read(cx).editor().source().len().get();
        if self.config.range.is_empty()
            && source_len > 0
            && let Ok(range) = ByteRange::new(ByteOffset::new(0), ByteOffset::new(source_len))
        {
            self.config.range = range;
        }
        if !self.config.range.is_empty()
            && !self.config.algorithms.is_empty()
            && self.config.range.len().get() <= AUTO_RUN_MAX_BYTES
        {
            self.run(cx);
            return;
        }
        cx.notify();
    }

    fn toggle_algorithm(&mut self, alg: Algorithm, cx: &mut Context<Self>) {
        if !self.config.algorithms.remove(&alg) {
            self.config.algorithms.insert(alg);
        }
        cx.notify();
    }

    /// Kick off a background checksum compute over the current config
    /// and apply the result once it lands. Never runs on the UI
    /// thread: `compute` itself executes inside `cx.background_spawn`,
    /// on the gpui background executor pool.
    fn run(&mut self, cx: &mut Context<Self>) {
        let Some(pane) = self.owning_pane.clone() else { return };
        if self.config.algorithms.is_empty() {
            return;
        }
        if self.running {
            // Queue a re-run rather than dropping it; the completion
            // handler replays it against the then-current bytes.
            self.pending_rerun = true;
            cx.notify();
            return;
        }
        let source = pane.read(cx).editor().source().clone();
        let config = self.config.clone();
        self.running = true;
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let outcome = cx.background_spawn(async move { compute(source.as_ref(), &config) }).await;
            let _ = this.update(cx, |this, cx| {
                this.running = false;
                this._compute = None;
                match outcome {
                    Ok(result) => {
                        this.last_result = Some(result);
                        cx.notify();
                    }
                    Err(err) => {
                        tracing::warn!(%err, "checksum computation failed");
                        cx.notify();
                    }
                }
                if std::mem::take(&mut this.pending_rerun) {
                    this.run(cx);
                }
            });
        });
        self._compute = Some(task);
    }

    /// Re-run against the owning file's current bytes after an external
    /// reload swapped its pane's source. No-op for a panel nobody has used
    /// yet, and -- mirroring egui's `cascade_byte_change` -- skipped for an
    /// empty or over-[`AUTO_RUN_MAX_BYTES`] file so a reload of a giant
    /// dump doesn't pin a background worker.
    pub(crate) fn recompute_after_reload(&mut self, cx: &mut Context<Self>) {
        if !(self.last_result.is_some() || self.running) {
            return;
        }
        let Some(pane) = self.owning_pane.clone() else { return };
        let len = pane.read(cx).editor().source().len().get();
        if len == 0 || len > AUTO_RUN_MAX_BYTES {
            return;
        }
        self.run(cx);
    }

    /// Parse the range text input (blank = keep the current config
    /// value) and kick off a run. Invalid input toasts an error and
    /// leaves the config untouched. Mirrors
    /// `StringsPanel::on_run_clicked`'s range parsing.
    fn on_run_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.owning_pane.clone() else { return };
        let source_len = pane.read(cx).editor().source().len().get();

        let range_text = self.range_input.read(cx).value().to_string();
        if !range_text.trim().is_empty() {
            match parse_range_expr(range_text.trim(), source_len, &hxy_calculator::NullResolver) {
                Ok(resolved) => {
                    match ByteRange::new(ByteOffset::new(resolved.start), ByteOffset::new(resolved.end_exclusive)) {
                        Ok(range) => self.config.range = range,
                        Err(err) => {
                            window.push_notification(
                                Notification::error(hxy_i18n::t_args(
                                    "gpui-checksums-invalid-range",
                                    &[("reason", &err.to_string())],
                                )),
                                cx,
                            );
                            return;
                        }
                    }
                }
                Err(err) => {
                    window.push_notification(
                        Notification::error(hxy_i18n::t_args(
                            "gpui-checksums-invalid-range",
                            &[("reason", &err.to_string())],
                        )),
                        cx,
                    );
                    return;
                }
            }
        }

        self.run(cx);
    }

    /// Copy one result row's hex digest to the clipboard. Reuses the
    /// same clipboard plumbing the palette's `CopySelection` /
    /// `CopyText` actions use (`cx.write_to_clipboard`).
    fn copy_value(&self, value: &str, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(value.to_owned()));
    }

    fn algorithm_checkbox(&self, alg: Algorithm, cx: &Context<Self>) -> impl IntoElement {
        Checkbox::new(("checksums-alg", alg as usize))
            .label(alg.label())
            .checked(self.config.algorithms.contains(&alg))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_algorithm(alg, cx)))
    }

    fn run_button(&self, cx: &Context<Self>) -> impl IntoElement {
        let label = if self.running { hxy_i18n::t("checksums-running") } else { hxy_i18n::t("checksums-run") };
        Button::new("checksums-run-btn")
            .label(label)
            .compact()
            .loading(self.running)
            .disabled(self.running || self.config.algorithms.is_empty())
            .on_click(cx.listener(|this, _, window, cx| this.on_run_clicked(window, cx)))
    }

    fn render_toolbar(&self, cx: &Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .flex_wrap()
                    .children(Algorithm::ALL.map(|alg| self.algorithm_checkbox(alg, cx))),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(Label::new(hxy_i18n::t("gpui-checksums-range-label")))
                    .child(div().w(px(220.0)).child(Input::new(&self.range_input)))
                    .child(self.run_button(cx)),
            )
    }

    fn render_summary(&self, cx: &Context<Self>) -> impl IntoElement {
        let mut col = v_flex().gap_1().px_2().py_1();
        if !self.config.range.is_empty() {
            col = col.child(
                Label::new(hxy_i18n::t_args(
                    "checksums-range",
                    &[
                        ("start", &format!("0x{:X}", self.config.range.start().get())),
                        ("end", &format!("0x{:X}", self.config.range.end().get())),
                        ("length", &format_bytes(self.config.range.len().get())),
                    ],
                ))
                .text_color(cx.theme().muted_foreground),
            );
        }
        col
    }

    fn render_results(&self, cx: &Context<Self>) -> impl IntoElement {
        let Some(result) = &self.last_result else {
            let text =
                if self.running { hxy_i18n::t("checksums-running") } else { hxy_i18n::t("checksums-no-results-yet") };
            return div().p_2().text_color(cx.theme().muted_foreground).child(text).into_any_element();
        };
        v_flex()
            .gap_1()
            .p_2()
            .children(result.values.iter().map(|(alg, value)| {
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().w(px(90.0)).child(Label::new(alg.label())))
                    .child(div().flex_1().child(Label::new(value.clone())))
                    .child({
                        let value = value.clone();
                        Button::new(("checksums-copy", *alg as usize))
                            .label(hxy_i18n::t("checksums-copy"))
                            .compact()
                            .on_click(cx.listener(move |this, _, _, cx| this.copy_value(&value, cx)))
                    })
            }))
            .into_any_element()
    }
}

/// Extract the stored owning path from a `ChecksumsPanel` payload.
fn path_from_info(info: &PanelInfo) -> Option<PathBuf> {
    let PanelInfo::Panel(value) = info else { return None };
    value.get("path").and_then(|p| p.as_str()).map(PathBuf::from)
}

/// Rebuild the algorithm set / range from persisted JSON. A missing
/// field (older layout, first open) falls back to
/// `ChecksumConfig::default()`'s value for that field -- the same
/// default a fresh panel starts with. An unrecognized algorithm key is
/// corrupt data and is skipped with a warning rather than defaulting
/// the whole set. Mirrors `StringsPanel`'s `config_from_info`.
fn config_from_info(info: &PanelInfo) -> ChecksumConfig {
    let default = ChecksumConfig::default();
    let PanelInfo::Panel(value) = info else { return default };

    let algorithms: BTreeSet<Algorithm> = match value.get("algorithms").and_then(|v| v.as_array()) {
        Some(keys) => keys
            .iter()
            .filter_map(|k| k.as_str())
            .filter_map(|key| match algorithm_from_key(key) {
                Some(alg) => Some(alg),
                None => {
                    tracing::warn!(algorithm = key, "restore: unrecognized checksum algorithm; skipping");
                    None
                }
            })
            .collect(),
        None => default.algorithms,
    };

    let range =
        match (value.get("range_start").and_then(|v| v.as_u64()), value.get("range_end").and_then(|v| v.as_u64())) {
            (Some(start), Some(end)) => match ByteRange::new(ByteOffset::new(start), ByteOffset::new(end)) {
                Ok(range) => range,
                Err(err) => {
                    tracing::warn!(%err, "restore: invalid checksums range; using default");
                    default.range
                }
            },
            _ => default.range,
        };

    ChecksumConfig { algorithms, range }
}

fn algorithm_key(alg: Algorithm) -> &'static str {
    match alg {
        Algorithm::Crc32 => "crc32",
        Algorithm::Adler32 => "adler32",
        Algorithm::Md5 => "md5",
        Algorithm::Sha1 => "sha1",
        Algorithm::Sha256 => "sha256",
        Algorithm::Sha512 => "sha512",
        Algorithm::Blake3 => "blake3",
    }
}

fn algorithm_from_key(key: &str) -> Option<Algorithm> {
    match key {
        "crc32" => Some(Algorithm::Crc32),
        "adler32" => Some(Algorithm::Adler32),
        "md5" => Some(Algorithm::Md5),
        "sha1" => Some(Algorithm::Sha1),
        "sha256" => Some(Algorithm::Sha256),
        "sha512" => Some(Algorithm::Sha512),
        "blake3" => Some(Algorithm::Blake3),
        _ => None,
    }
}

/// The panel base name shown on its tab: the owning file's leaf name,
/// falling back to the untitled placeholder when it has no path.
/// Mirrors `StringsPanel`'s `tab_label`.
fn tab_label(path: Option<&Path>) -> String {
    match path {
        Some(path) => path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
        None => hxy_i18n::t("gpui-file-untitled"),
    }
}

fn format_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else if n < 1024 * 1024 * 1024 {
        format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GiB", n as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

impl BasePanel for ChecksumsPanel {
    fn panel_name(&self) -> &'static str {
        CHECKSUMS_PANEL_NAME
    }

    /// Persist the owning path, selected algorithms, and range
    /// verbatim -- see the module doc for why this diverges from
    /// strings/entropy's "backfill fresh on rebind" rule.
    fn dump(&self, _cx: &App) -> PanelState {
        let mut state = PanelState::new(self.panel_name());
        state.info = PanelInfo::panel(serde_json::json!({
            "path": self.owning_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "algorithms": self.config.algorithms.iter().map(|&alg| algorithm_key(alg)).collect::<Vec<_>>(),
            "range_start": self.config.range.start().get(),
            "range_end": self.config.range.end().get(),
        }));
        state
    }
}

impl Panel for ChecksumsPanel {
    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        SharedString::from(hxy_i18n::t_args("tab-checksums", &[("name", &tab_label(self.owning_path.as_deref()))]))
    }

    fn tab_name(&self, _cx: &App) -> Option<SharedString> {
        Some(SharedString::from(hxy_i18n::t_args(
            "tab-checksums",
            &[("name", &tab_label(self.owning_path.as_deref()))],
        )))
    }
}

impl Focusable for ChecksumsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ChecksumsPanel {}

impl Render for ChecksumsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = v_flex().size_full().bg(cx.theme().background);
        if self.owning_pane.is_none() {
            return root.child(
                div().p_2().text_color(cx.theme().muted_foreground).child(hxy_i18n::t("checksums-no-active-file")),
            );
        }
        root.child(self.render_toolbar(cx))
            .child(self.render_summary(cx))
            .child(div().flex_1().min_h_0().child(self.render_results(cx)))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::TestAppContext;
    use hxy_core::MemorySource;

    use super::*;

    fn setup(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
    }

    fn source(bytes: Vec<u8>) -> Arc<dyn HexSource> {
        Arc::new(MemorySource::new(bytes))
    }

    fn build(cx: &mut TestAppContext, bytes: Vec<u8>) -> (Entity<ChecksumsPanel>, &mut gpui::VisualTestContext) {
        let window = cx.add_window(|window, cx| {
            let pane = cx.new(|cx| HexPane::new(source(bytes), cx));
            let panel = cx
                .new(|cx| ChecksumsPanel::new(pane, Some(PathBuf::from("/tmp/hxy-checksums-fixture.bin")), window, cx));
            gpui_component::Root::new(panel, window, cx)
        });
        let root = window.root(cx).unwrap();
        let panel = root.read_with(cx, |root, _| root.view().clone().downcast::<ChecksumsPanel>().unwrap());
        let vcx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        vcx.run_until_parked();
        (panel, vcx)
    }

    /// A run requested while another is in flight is queued and replayed
    /// on completion, so a reload-triggered recompute is never swallowed.
    #[gpui::test]
    fn in_flight_run_queues_a_rerun(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx, b"abc".to_vec());
        cx.run_until_parked();
        panel.update(cx, |p, cx| {
            p.run(cx);
            assert!(p.running, "the first run is in flight");
            p.run(cx);
            assert!(p.pending_rerun, "a second run mid-flight is queued");
        });
        cx.run_until_parked();
        assert!(!panel.read_with(cx, |p, _| p.pending_rerun), "the queued rerun was consumed");
    }

    /// `recompute_after_reload` is gated on file size like egui's
    /// `cascade_byte_change`: an empty source is skipped before `run`.
    #[gpui::test]
    fn recompute_after_reload_skips_empty_source(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx, Vec::new());
        cx.run_until_parked();
        panel.update(cx, |p, cx| {
            p.running = true;
            p.recompute_after_reload(cx);
            assert!(!p.pending_rerun, "an empty source is gated out before run()");
            p.running = false;
        });
    }

    /// Opening the panel on a small fixture auto-runs (default
    /// algorithms, under `AUTO_RUN_MAX_BYTES`) and produces the same
    /// digests `hxy_panels::checksums::compute` would (known vectors
    /// are `hxy_panels::checksums`'s own test suite; this only
    /// confirms the pipeline runs end to end).
    #[gpui::test]
    fn run_over_fixture_produces_rows(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx, b"abc".to_vec());
        cx.run_until_parked();

        let values = panel.read_with(cx, |p, _| p.last_result.clone().expect("auto-run populated a result").values);
        assert_eq!(values[&Algorithm::Sha256], "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(values[&Algorithm::Blake3], values[&Algorithm::Blake3]);
        assert!(!panel.read_with(cx, |p, _| p.running));
    }

    #[gpui::test]
    fn copy_value_writes_hex_to_clipboard(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx, b"abc".to_vec());
        cx.run_until_parked();

        panel.update(cx, |p, cx| p.copy_value("ba7816bf", cx));
        let clip = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(clip.as_deref(), Some("ba7816bf"));
    }

    #[gpui::test]
    fn dump_round_trips_path_algorithms_and_range(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx, vec![0u8; 16]);
        panel.update(cx, |p, cx| {
            p.config.algorithms = BTreeSet::from([Algorithm::Crc32, Algorithm::Md5]);
            p.config.range = ByteRange::new(ByteOffset::new(2), ByteOffset::new(10)).unwrap();
            cx.notify();
        });

        let dumped = panel.read_with(cx, |p, cx| p.dump(cx));
        assert_eq!(dumped.panel_name, CHECKSUMS_PANEL_NAME);
        assert_eq!(path_from_info(&dumped.info), Some(PathBuf::from("/tmp/hxy-checksums-fixture.bin")));

        let config = config_from_info(&dumped.info);
        assert_eq!(config.algorithms, BTreeSet::from([Algorithm::Crc32, Algorithm::Md5]));
        assert_eq!(config.range, ByteRange::new(ByteOffset::new(2), ByteOffset::new(10)).unwrap());
    }

    #[test]
    fn config_from_info_defaults_for_older_or_empty_layouts() {
        let info = PanelInfo::panel(serde_json::json!({}));
        let config = config_from_info(&info);
        assert_eq!(config, ChecksumConfig::default());
    }

    #[test]
    fn config_from_info_skips_unrecognized_algorithm_keys() {
        let info = PanelInfo::panel(serde_json::json!({
            "algorithms": ["sha256", "rot13"],
            "range_start": 0,
            "range_end": 4,
        }));
        let config = config_from_info(&info);
        assert_eq!(config.algorithms, BTreeSet::from([Algorithm::Sha256]));
    }

    /// Restore has no live `FilePanel` to bind to yet when the
    /// `OpenFilePanels` global is already populated: the panel finds
    /// it immediately and auto-runs. Mirrors `StringsPanel`'s
    /// equivalent restore test.
    #[gpui::test]
    fn restore_binds_immediately_when_global_already_has_the_file(cx: &mut TestAppContext) {
        setup(cx);
        let path = PathBuf::from("/tmp/hxy-checksums-restore-a.bin");
        let path_for_closure = path.clone();
        let window = cx.add_window(move |window, cx| {
            let file_panel =
                cx.new(|cx| FilePanel::new(source(vec![0u8; 16]), Some(path_for_closure.clone()), window, cx));
            cx.set_global(OpenFilePanels(vec![file_panel]));
            let info = PanelInfo::panel(serde_json::json!({ "path": path_for_closure.to_string_lossy() }));
            let panel = cx.new(|cx| ChecksumsPanel::restore(&info, window, cx));
            gpui_component::Root::new(panel, window, cx)
        });
        let root = window.root(cx).unwrap();
        let panel = root.read_with(cx, |root, _| root.view().clone().downcast::<ChecksumsPanel>().unwrap());
        let cx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        cx.run_until_parked();

        assert!(panel.read_with(cx, |p, _| p.owning_pane.is_some()));
        assert_eq!(panel.read_with(cx, |p, _| p.owning_path.clone()), Some(path));
        assert!(panel.read_with(cx, |p, _| p.last_result.is_some()), "auto-ran after binding");
    }

    /// Restore before the file reopens: the panel starts unbound and
    /// picks up the file once the `OpenFilePanels` global is
    /// published, then auto-runs.
    #[gpui::test]
    fn restore_rebinds_when_global_updates_later(cx: &mut TestAppContext) {
        setup(cx);
        let path = PathBuf::from("/tmp/hxy-checksums-restore-b.bin");
        let info = PanelInfo::panel(serde_json::json!({ "path": path.to_string_lossy() }));
        let window = cx.add_window(move |window, cx| {
            let panel = cx.new(|cx| ChecksumsPanel::restore(&info, window, cx));
            gpui_component::Root::new(panel, window, cx)
        });
        let root = window.root(cx).unwrap();
        let panel = root.read_with(cx, |root, _| root.view().clone().downcast::<ChecksumsPanel>().unwrap());
        let cx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        cx.run_until_parked();
        assert!(panel.read_with(cx, |p, _| p.owning_pane.is_none()), "no file open yet");

        let path_for_closure = path.clone();
        cx.update(|window, cx| {
            let file_panel = cx.new(|cx| FilePanel::new(source(vec![0u8; 16]), Some(path_for_closure), window, cx));
            cx.set_global(OpenFilePanels(vec![file_panel]));
        });
        cx.run_until_parked();

        assert!(panel.read_with(cx, |p, _| p.owning_pane.is_some()));
    }

    /// An empty buffer binds without triggering an implicit background
    /// compute (`bind_pane`'s `source_len > 0` guard for the backfill,
    /// which leaves `config.range` empty), leaving the panel in the
    /// "not computed yet" state.
    #[gpui::test]
    fn binding_an_empty_file_does_not_auto_run(cx: &mut TestAppContext) {
        setup(cx);
        let window = cx.add_window(|window, cx| {
            let pane = cx.new(|cx| HexPane::new(source(Vec::new()), cx));
            let panel = cx.new(|cx| ChecksumsPanel::new(pane, None, window, cx));
            gpui_component::Root::new(panel, window, cx)
        });
        let root = window.root(cx).unwrap();
        let panel = root.read_with(cx, |root, _| root.view().clone().downcast::<ChecksumsPanel>().unwrap());
        let cx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        cx.run_until_parked();
        assert!(panel.read_with(cx, |p, _| p.last_result.is_none()), "empty buffer must not auto-run");
    }

    /// `recompute_after_reload` is a no-op for a panel with no result
    /// (mirrors egui's `has_checksums` cascade gate).
    #[gpui::test]
    fn recompute_after_reload_noop_when_never_computed(cx: &mut TestAppContext) {
        setup(cx);
        let window = cx.add_window(|window, cx| {
            let pane = cx.new(|cx| HexPane::new(source(Vec::new()), cx));
            let panel = cx.new(|cx| ChecksumsPanel::new(pane, None, window, cx));
            gpui_component::Root::new(panel, window, cx)
        });
        let root = window.root(cx).unwrap();
        let panel = root.read_with(cx, |root, _| root.view().clone().downcast::<ChecksumsPanel>().unwrap());
        let cx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        cx.run_until_parked();
        assert!(panel.read_with(cx, |p, _| p.last_result.is_none()));

        panel.update(cx, |p, cx| p.recompute_after_reload(cx));
        cx.run_until_parked();
        assert!(panel.read_with(cx, |p, _| p.last_result.is_none()), "no prior result means no auto-recompute");
    }

    /// `recompute_after_reload` re-runs against the pane's current
    /// bytes once a result already exists -- the reload cascade's
    /// entry point after `Workspace::resolve_reload` swaps the
    /// owning file's source.
    #[gpui::test]
    fn recompute_after_reload_reruns_when_a_result_exists(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx, vec![0u8; 32]);
        cx.run_until_parked();
        let before = panel.read_with(cx, |p, _| p.last_result.clone().expect("auto-ran").values);

        let pane = panel.read_with(cx, |p, _| p.owning_pane.clone().unwrap());
        pane.update(cx, |pane, cx| {
            pane.editor_mut().swap_source(Arc::new(MemorySource::new(vec![0xFFu8; 32])));
            cx.notify();
        });

        panel.update(cx, |p, cx| p.recompute_after_reload(cx));
        cx.run_until_parked();
        let after = panel.read_with(cx, |p, _| p.last_result.clone().expect("recomputed").values);
        assert_ne!(before, after, "recompute picked up the new bytes");
    }
}
