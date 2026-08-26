//! [`EntropyPanel`]: per-file Shannon-entropy plot tab.
//!
//! Computation lives entirely in `hxy_panels::entropy` (shared with the
//! egui front end); this module only owns background dispatch, the
//! toolbar (Compute/Recompute button plus a mean/max/window readout),
//! and a small custom chart built from gpui-component's `plot`
//! primitives.
//!
//! Binding shape mirrors [`StringsPanel`](super::StringsPanel) (see
//! that module's doc for the restore/rebind rationale): a per-file
//! center-dock tab keyed by the owning file's path, bound eagerly when
//! opened from the palette / View menu, or lazily via the
//! [`OpenFilePanels`](super::strings::OpenFilePanels) global when
//! restored from a persisted layout. Unlike strings, there is no
//! results table, no config to persist beyond the owning path, and no
//! click-to-jump (egui's entropy panel has none either -- a shared
//! M4+ enhancement candidate, not built here). Auto-run also differs
//! from strings: entropy computes unconditionally for any non-empty
//! file, with no size cap -- see [`EntropyPanel::bind_pane`]'s doc for
//! why (egui has no such gate for entropy either).
//!
//! Chart route: gpui-component 0.5.1's packaged `LineChart`/`AreaChart`
//! auto-fit their Y domain to the data (`ScaleLinear::new` always
//! chains in `Y::zero()`, never a fixed max) and their X scale is
//! `ScalePoint` (categorical, one label slot per data point, formatted
//! by a user closure into a display string). Neither fits egui
//! parity's fixed `[0, 8]` Y range or a numeric hex-offset X axis with
//! a handful of evenly spaced ticks independent of point density
//! (entropy plots can carry up to `TARGET_POINTS` = 4096 samples). So
//! [`EntropyChart`] assembles the same primitives those charts are
//! built from directly (`plot::shape::Line`, `plot::scale::ScaleLinear`,
//! `plot::PlotAxis`, `plot::Grid`, `plot::label::Text`), the way
//! `LineChart::paint` itself does, rather than hand-rolling a raw
//! canvas paint against `hxy-view-gpui`'s primitives.

use std::path::Path;
use std::path::PathBuf;

use gpui::App;
use gpui::AppContext;
use gpui::Bounds;
use gpui::Context;
use gpui::Entity;
use gpui::EventEmitter;
use gpui::FocusHandle;
use gpui::Focusable;
use gpui::IntoElement;
use gpui::ParentElement;
use gpui::Pixels;
use gpui::Render;
use gpui::SharedString;
use gpui::Styled;
use gpui::Subscription;
use gpui::Task;
use gpui::TextAlign;
use gpui::Window;
use gpui::bounds;
use gpui::div;
use gpui::point;
use gpui::px;
use gpui::size;
use gpui::component::ActiveTheme;
use gpui::component::Disableable;
use gpui::component::button::Button;
use gpui::component::dock::BasePanel;
use gpui::component::dock::Panel;
use gpui::component::dock::PanelEvent;
use gpui::component::dock::PanelInfo;
use gpui::component::dock::PanelState;
use gpui::component::h_flex;
use gpui::component::label::Label;
use gpui::component::plot::AXIS_GAP;
use gpui::component::plot::AxisText;
use gpui::component::plot::Grid;
use gpui::component::plot::IntoPlot;
use gpui::component::plot::Plot;
use gpui::component::plot::PlotAxis;
use gpui::component::plot::PlotLabel;
use gpui::component::plot::label::Text as PlotText;
use gpui::component::plot::scale::Scale;
use gpui::component::plot::scale::ScaleLinear;
use gpui::component::plot::shape::Line;
use gpui::component::v_flex;
use hxy_core::HexSource;
use hxy_panels::entropy::EntropyPoint;
use hxy_panels::entropy::EntropyState;
use hxy_panels::entropy::MAX_ENTROPY;
use hxy_panels::entropy::compute_entropy;
use hxy_panels::entropy::pick_window_size;
use hxy_view_gpui::HexPane;

use super::FilePanel;
use super::strings::OpenFilePanels;

/// Stable identifier for layout (de)serialization; must never change.
pub const ENTROPY_PANEL_NAME: &str = "EntropyPanel";

/// The currently published set of open file panels, or an empty list
/// before the workspace has published one yet. Reuses
/// [`OpenFilePanels`], the same global `StringsPanel`'s restore-time
/// rebind observes -- both panel kinds bind to a file by path, so
/// there is exactly one "open files by path" registry, not one per
/// panel kind.
fn open_file_panels(cx: &App) -> Vec<Entity<FilePanel>> {
    match cx.try_global::<OpenFilePanels>() {
        Some(g) => g.0.clone(),
        None => Vec::new(),
    }
}

/// Reload-cascade size cap. Entropy auto-runs ungated on initial open
/// (`bind_pane`), but egui's `cascade_byte_change` gates the reload
/// recompute -- entropy included -- by `len == 0 || len > this`, so a
/// reload of a giant dump doesn't pin a background worker.
const AUTO_RUN_MAX_BYTES: u64 = 256 * 1024 * 1024;

pub struct EntropyPanel {
    focus_handle: FocusHandle,
    owning_path: Option<PathBuf>,
    owning_pane: Option<Entity<HexPane>>,
    _rebind_observe: Subscription,
    state: Option<EntropyState>,
    running: bool,
    /// A run requested while another was in flight; replayed on
    /// completion so a reload-triggered recompute is never swallowed.
    pending_rerun: bool,
    _compute: Option<Task<()>>,
}

impl EntropyPanel {
    /// Build a fresh panel bound immediately to `pane` (the palette /
    /// View-menu open path, where the owning file is already live).
    pub fn new(pane: Entity<HexPane>, path: Option<PathBuf>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self::with_state(path, cx);
        this.bind_pane(pane, cx);
        this
    }

    /// Rebuild from persisted [`PanelInfo`]. No live file to bind to
    /// yet (see module doc); binds lazily via [`OpenFilePanels`].
    pub fn restore(info: &PanelInfo, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path = path_from_info(info);
        let mut this = Self::with_state(path, cx);
        this.try_bind_from_global(cx);
        this
    }

    fn with_state(path: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let rebind_observe = cx.observe_global::<OpenFilePanels>(|this, cx| this.try_bind_from_global(cx));
        Self {
            focus_handle: cx.focus_handle(),
            owning_path: path,
            owning_pane: None,
            _rebind_observe: rebind_observe,
            state: None,
            running: false,
            pending_rerun: false,
            _compute: None,
        }
    }

    /// The file path this panel scans, for the workspace's open-tab
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

    /// Bind to the owning file's pane and run unconditionally for any
    /// non-empty file with no result yet -- egui parity:
    /// `compute_entropy_for` (`crates/hxy/src/app/mod.rs`) has no size
    /// gate at all (that gate belongs to strings/checksums, whose
    /// extractors are not a single O(n) pass); it only skips an empty
    /// buffer. `StringsPanel::bind_pane`'s `AUTO_RUN_MAX_BYTES` cap does
    /// not apply here.
    fn bind_pane(&mut self, pane: Entity<HexPane>, cx: &mut Context<Self>) {
        if self.owning_pane.is_some() {
            return;
        }
        self.owning_pane = Some(pane.clone());
        let source_len = pane.read(cx).editor().source().len().get();
        if self.state.is_none() && source_len > 0 {
            self.run(cx);
            return;
        }
        cx.notify();
    }

    /// Kick off a background entropy scan over the owning file's
    /// current bytes and apply the result once it lands. `compute_entropy`
    /// itself never runs on the UI thread: it executes inside
    /// `cx.background_spawn`, on the gpui background executor pool.
    fn run(&mut self, cx: &mut Context<Self>) {
        let Some(pane) = self.owning_pane.clone() else { return };
        if self.running {
            // Queue a re-run rather than dropping it; the completion
            // handler replays it against the then-current bytes.
            self.pending_rerun = true;
            cx.notify();
            return;
        }
        let source = pane.read(cx).editor().source().clone();
        let source_len = source.len().get();
        let window_bytes = pick_window_size(source_len);
        self.running = true;
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let outcome = cx.background_spawn(async move { compute_entropy(source.as_ref(), window_bytes) }).await;
            let _ = this.update(cx, |this, cx| {
                this.running = false;
                this._compute = None;
                match outcome {
                    Ok(points) => {
                        this.state = Some(EntropyState {
                            points,
                            source_len,
                            window_bytes,
                            computed_at: jiff::Timestamp::now(),
                        });
                        cx.notify();
                    }
                    Err(err) => {
                        tracing::warn!(%err, "entropy computation failed");
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
    /// yet, and -- mirroring egui's `cascade_byte_change`, which gates the
    /// reload recompute (entropy included) even though the initial
    /// `compute_entropy_for` is ungated -- skipped for an empty or
    /// over-[`AUTO_RUN_MAX_BYTES`] file.
    pub(crate) fn recompute_after_reload(&mut self, cx: &mut Context<Self>) {
        if !(self.state.is_some() || self.running) {
            return;
        }
        let Some(pane) = self.owning_pane.clone() else { return };
        let len = pane.read(cx).editor().source().len().get();
        if len == 0 || len > AUTO_RUN_MAX_BYTES {
            return;
        }
        self.run(cx);
    }

    fn compute_button(&self, cx: &Context<Self>) -> impl IntoElement {
        let label = if self.running {
            hxy_i18n::t("entropy-computing")
        } else if self.state.is_some() {
            hxy_i18n::t("entropy-recompute")
        } else {
            hxy_i18n::t("entropy-compute")
        };
        Button::new("entropy-compute-btn")
            .label(label)
            .compact()
            .loading(self.running)
            .disabled(self.running)
            .on_click(cx.listener(|this, _, _, cx| this.run(cx)))
    }

    fn render_toolbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let mut row = h_flex().gap_2().items_center().px_2().py_1().child(self.compute_button(cx));
        if let Some(state) = &self.state {
            row = row.child(Label::new(format_summary(state)).text_color(cx.theme().muted_foreground));
        }
        v_flex().border_b_1().border_color(cx.theme().border).child(row)
    }

    /// The plot area's body: the "not computed yet" / "empty buffer"
    /// placeholders mirror egui's `panels::entropy::show` exactly (both
    /// checked regardless of `running`, since a background compute in
    /// flight has nothing to show until it lands), or the chart itself.
    fn render_body(&self, cx: &Context<Self>) -> impl IntoElement {
        let Some(state) = &self.state else {
            return div()
                .p_2()
                .text_color(cx.theme().muted_foreground)
                .child(hxy_i18n::t("entropy-empty"))
                .into_any_element();
        };
        if state.points.is_empty() {
            return div()
                .p_2()
                .text_color(cx.theme().muted_foreground)
                .child(hxy_i18n::t("entropy-zero-bytes"))
                .into_any_element();
        }
        // Non-empty per the `is_empty` check above, so this always has a
        // last point; its end offset (not just its start) is the X
        // domain's upper bound, so the line reaches the right edge.
        let last = state.points.last().expect("checked non-empty above");
        let max_offset = (last.offset + state.window_bytes) as f64;
        EntropyChart { points: state.points.clone(), window_bytes: state.window_bytes, max_offset }.into_any_element()
    }
}

/// Extract the stored owning path from an `EntropyPanel` payload.
fn path_from_info(info: &PanelInfo) -> Option<PathBuf> {
    let PanelInfo::Panel(value) = info else { return None };
    value.get("path").and_then(|p| p.as_str()).map(PathBuf::from)
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

fn format_summary(state: &EntropyState) -> String {
    hxy_i18n::t_args(
        "entropy-summary",
        &[
            ("mean", &format!("{:.2}", state.mean())),
            ("max", &format!("{:.2}", state.max())),
            ("window", &format_bytes(state.window_bytes)),
            ("count", &state.points.len().to_string()),
        ],
    )
}

fn format_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
    }
}

impl BasePanel for EntropyPanel {
    fn panel_name(&self) -> &'static str {
        ENTROPY_PANEL_NAME
    }

    /// Persist the owning path only. The computed points, mean/max, and
    /// window size are all data-dependent (tied to the file's current
    /// bytes) and re-backfill via auto-run on rebind rather than
    /// round-tripping verbatim -- same rule `StringsPanel::dump`
    /// documents for its scan range.
    fn dump(&self, _cx: &App) -> PanelState {
        let mut state = PanelState::new(self.panel_name());
        state.info = PanelInfo::panel(serde_json::json!({
            "path": self.owning_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
        }));
        state
    }
}

impl Panel for EntropyPanel {
    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        SharedString::from(hxy_i18n::t_args("tab-entropy", &[("name", &tab_label(self.owning_path.as_deref()))]))
    }

    fn tab_name(&self, _cx: &App) -> Option<SharedString> {
        Some(SharedString::from(hxy_i18n::t_args("tab-entropy", &[("name", &tab_label(self.owning_path.as_deref()))])))
    }
}

impl Focusable for EntropyPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for EntropyPanel {}

impl Render for EntropyPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = v_flex().size_full().bg(cx.theme().background);
        if self.owning_pane.is_none() {
            return root.child(
                div().p_2().text_color(cx.theme().muted_foreground).child(hxy_i18n::t("entropy-no-active-file")),
            );
        }
        root.child(self.render_toolbar(cx)).child(div().flex_1().min_h_0().p_2().child(self.render_body(cx)))
    }
}

/// A fixed-`[0, MAX_ENTROPY]`-Y-domain, hex-offset-X-axis line plot
/// over one file's entropy samples. See the module doc for why this
/// assembles gpui-component's `plot` primitives directly instead of
/// using `chart::LineChart`.
#[derive(IntoPlot)]
struct EntropyChart {
    points: Vec<EntropyPoint>,
    window_bytes: u64,
    /// Upper bound of the X domain: the last window's end offset (not
    /// just its start), so the line reaches the right edge.
    max_offset: f64,
}

/// Evenly spaced X-axis tick count, independent of point density (up
/// to `TARGET_POINTS` = 4096 samples would otherwise each claim a
/// label slot, as `LineChart`'s per-point `ScalePoint` ticks do).
const X_TICKS: usize = 6;

/// Y gridline / label rows: five even steps across the fixed
/// `[0, MAX_ENTROPY]` domain (derived from the constant, not a literal
/// `8.0`, so the two can never drift apart).
const Y_TICKS: [f64; 5] = [0.0, MAX_ENTROPY / 4.0, MAX_ENTROPY / 2.0, MAX_ENTROPY * 3.0 / 4.0, MAX_ENTROPY];

impl Plot for EntropyChart {
    fn paint(&mut self, plot_bounds: Bounds<Pixels>, window: &mut gpui::Window, cx: &mut App) {
        if self.points.is_empty() {
            return;
        }
        let width = plot_bounds.size.width.as_f32();
        let height = plot_bounds.size.height.as_f32() - AXIS_GAP;

        // Reserve a left margin for the fixed Y-axis value labels
        // (drawn by hand below, not via `PlotAxis::y_label`, which
        // places text to the right of the axis line -- inside the
        // plot area, where it would sit on top of the data).
        let left_margin = 22.0_f32;
        let chart_bounds = bounds(
            plot_bounds.origin + point(px(left_margin), px(0.)),
            size(px(width - left_margin), plot_bounds.size.height),
        );
        let plot_width = width - left_margin;

        let x = ScaleLinear::new(vec![0.0_f64, self.max_offset], vec![0., plot_width]);
        let y = ScaleLinear::new(vec![0.0_f64, MAX_ENTROPY], vec![height, 10.]);

        let x_label = (0..=X_TICKS).filter_map(|i| {
            let value = self.max_offset * (i as f64 / X_TICKS as f64);
            x.tick(&value).map(|tick| {
                let align = if i == 0 {
                    TextAlign::Left
                } else if i == X_TICKS {
                    TextAlign::Right
                } else {
                    TextAlign::Center
                };
                AxisText::new(format!("0x{:X}", value as u64), tick, cx.theme().muted_foreground).align(align)
            })
        });
        PlotAxis::new().x(height).x_label(x_label).stroke(cx.theme().border).paint(&chart_bounds, window, cx);

        Grid::new()
            .y(Y_TICKS.iter().filter_map(|v| y.tick(v)).map(px).collect::<Vec<_>>())
            .stroke(cx.theme().border)
            .dash_array(&[px(4.), px(2.)])
            .paint(&chart_bounds, window);

        let y_labels: Vec<PlotText> = Y_TICKS
            .iter()
            .filter_map(|v| {
                y.tick(v).map(|tick| {
                    PlotText::new(
                        format!("{v:.0}"),
                        point(px(-left_margin + 2.0), px(tick)),
                        cx.theme().muted_foreground,
                    )
                    .align(TextAlign::Right)
                })
            })
            .collect();
        PlotLabel::new(y_labels).paint(&chart_bounds, window, cx);

        // `x`/`y` aren't needed again after this, so the closures can
        // take ownership directly instead of cloning.
        let window_mid = self.window_bytes as f64 / 2.0;
        let line = Line::new()
            .data(self.points.iter().copied())
            // Centre each window's entropy at the window's midpoint so
            // the line lines up with the file region it summarizes
            // rather than the window's leading edge.
            .x(move |p: &EntropyPoint| x.tick(&(p.offset as f64 + window_mid)))
            .y(move |p: &EntropyPoint| y.tick(&p.entropy))
            .stroke(cx.theme().chart_2)
            .stroke_width(px(1.5));
        line.paint(&chart_bounds, window);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::TestAppContext;
    use hxy_core::MemorySource;
    use hxy_panels::entropy::MIN_WINDOW_BYTES;

    use super::*;

    fn setup(cx: &mut TestAppContext) {
        cx.update(gpui::component::init);
    }

    fn source(bytes: Vec<u8>) -> Arc<dyn HexSource> {
        Arc::new(MemorySource::new(bytes))
    }

    fn build(cx: &mut TestAppContext, bytes: Vec<u8>) -> (Entity<EntropyPanel>, &mut gpui::VisualTestContext) {
        let window = cx.add_window(|window, cx| {
            let pane = cx.new(|cx| HexPane::new(source(bytes), cx));
            let panel =
                cx.new(|cx| EntropyPanel::new(pane, Some(PathBuf::from("/tmp/hxy-entropy-fixture.bin")), window, cx));
            gpui::component::Root::new(panel, window, cx)
        });
        let root = window.root(cx).unwrap();
        let panel = root.read_with(cx, |root, _| root.view().clone().downcast::<EntropyPanel>().unwrap());
        let vcx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        vcx.run_until_parked();
        (panel, vcx)
    }

    /// A run requested while another is in flight is queued and replayed
    /// on completion, so a reload-triggered recompute is never swallowed.
    #[gpui::test]
    fn in_flight_run_queues_a_rerun(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx, vec![0xAAu8; 4096]);
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

    /// Entropy auto-runs ungated on the initial bind, but its reload
    /// recompute mirrors egui's `cascade_byte_change` size gate: an empty
    /// source is skipped before `run`.
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

    /// Opening the panel on a small fixture auto-runs unconditionally
    /// (no size gate -- see `bind_pane`'s doc) and produces exactly as
    /// many points as `pick_window_size` implies for that length -- the
    /// "compute integration" test from the task brief. Confirms the
    /// pipeline end to end without asserting on `compute_entropy`'s own
    /// math (that's `hxy_panels::entropy`'s test suite).
    #[gpui::test]
    fn auto_run_produces_the_expected_point_count(cx: &mut TestAppContext) {
        setup(cx);
        let len: u64 = 4096;
        let bytes = vec![0xAAu8; len as usize];
        let (panel, cx) = build(cx, bytes);
        cx.run_until_parked();

        let window_bytes = pick_window_size(len);
        assert_eq!(window_bytes, MIN_WINDOW_BYTES);
        let expected_points = len.div_ceil(window_bytes) as usize;

        let state = panel.read_with(cx, |p, _| p.state.clone().expect("auto-run populated a result"));
        assert_eq!(state.points.len(), expected_points);
        assert_eq!(state.window_bytes, window_bytes);
        assert!(!panel.read_with(cx, |p, _| p.running));
    }

    #[gpui::test]
    fn dump_round_trips_the_owning_path(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx, vec![0u8; 16]);

        let dumped = panel.read_with(cx, |p, cx| p.dump(cx));
        assert_eq!(dumped.panel_name, ENTROPY_PANEL_NAME);
        assert_eq!(path_from_info(&dumped.info), Some(PathBuf::from("/tmp/hxy-entropy-fixture.bin")));
    }

    #[test]
    fn path_from_info_defaults_to_none_for_older_or_empty_layouts() {
        let info = PanelInfo::panel(serde_json::json!({}));
        assert_eq!(path_from_info(&info), None);
    }

    /// Restore has no live `FilePanel` to bind to yet when the
    /// `OpenFilePanels` global is already populated: the panel finds
    /// it immediately and auto-runs. Mirrors `StringsPanel`'s
    /// equivalent restore test.
    #[gpui::test]
    fn restore_binds_immediately_when_global_already_has_the_file(cx: &mut TestAppContext) {
        setup(cx);
        let path = PathBuf::from("/tmp/hxy-entropy-restore-a.bin");
        let path_for_closure = path.clone();
        let window = cx.add_window(move |window, cx| {
            let file_panel =
                cx.new(|cx| FilePanel::new(source(vec![0u8; 16]), Some(path_for_closure.clone()), window, cx));
            cx.set_global(OpenFilePanels(vec![file_panel]));
            let info = PanelInfo::panel(serde_json::json!({ "path": path_for_closure.to_string_lossy() }));
            let panel = cx.new(|cx| EntropyPanel::restore(&info, window, cx));
            gpui::component::Root::new(panel, window, cx)
        });
        let root = window.root(cx).unwrap();
        let panel = root.read_with(cx, |root, _| root.view().clone().downcast::<EntropyPanel>().unwrap());
        let cx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        cx.run_until_parked();

        assert!(panel.read_with(cx, |p, _| p.owning_pane.is_some()));
        assert_eq!(panel.read_with(cx, |p, _| p.owning_path.clone()), Some(path));
        assert!(panel.read_with(cx, |p, _| p.state.is_some()), "auto-ran after binding");
    }

    /// Restore before the file reopens: the panel starts unbound and
    /// picks up the file once the `OpenFilePanels` global is
    /// published, then auto-runs.
    #[gpui::test]
    fn restore_rebinds_when_global_updates_later(cx: &mut TestAppContext) {
        setup(cx);
        let path = PathBuf::from("/tmp/hxy-entropy-restore-b.bin");
        let info = PanelInfo::panel(serde_json::json!({ "path": path.to_string_lossy() }));
        let window = cx.add_window(move |window, cx| {
            let panel = cx.new(|cx| EntropyPanel::restore(&info, window, cx));
            gpui::component::Root::new(panel, window, cx)
        });
        let root = window.root(cx).unwrap();
        let panel = root.read_with(cx, |root, _| root.view().clone().downcast::<EntropyPanel>().unwrap());
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
    /// scan (`bind_pane`'s `source_len > 0` guard), leaving the panel
    /// in the "not computed yet" state rather than a spurious empty
    /// result. A manual `run` (the compute button's dispatch path)
    /// still works afterward, producing the "zero bytes" state
    /// `compute_entropy` returns for an empty source.
    #[gpui::test]
    fn binding_an_empty_file_does_not_auto_run(cx: &mut TestAppContext) {
        setup(cx);
        let window = cx.add_window(|window, cx| {
            let pane = cx.new(|cx| HexPane::new(source(Vec::new()), cx));
            let panel = cx.new(|cx| EntropyPanel::new(pane, None, window, cx));
            gpui::component::Root::new(panel, window, cx)
        });
        let root = window.root(cx).unwrap();
        let panel = root.read_with(cx, |root, _| root.view().clone().downcast::<EntropyPanel>().unwrap());
        let cx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        cx.run_until_parked();
        assert!(panel.read_with(cx, |p, _| p.state.is_none()), "empty buffer must not auto-run");

        panel.update(cx, |p, cx| p.run(cx));
        cx.run_until_parked();
        let points_empty = panel.read_with(cx, |p, _| p.state.as_ref().map(|s| s.points.is_empty()));
        assert_eq!(points_empty, Some(true));
    }

    /// `recompute_after_reload` is a no-op for a panel nobody has
    /// looked at yet (mirrors egui's `has_entropy` cascade gate).
    #[gpui::test]
    fn recompute_after_reload_noop_when_never_computed(cx: &mut TestAppContext) {
        setup(cx);
        let window = cx.add_window(|window, cx| {
            let pane = cx.new(|cx| HexPane::new(source(Vec::new()), cx));
            let panel = cx.new(|cx| EntropyPanel::new(pane, None, window, cx));
            gpui::component::Root::new(panel, window, cx)
        });
        let root = window.root(cx).unwrap();
        let panel = root.read_with(cx, |root, _| root.view().clone().downcast::<EntropyPanel>().unwrap());
        let cx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        cx.run_until_parked();
        assert!(panel.read_with(cx, |p, _| p.state.is_none()));

        panel.update(cx, |p, cx| p.recompute_after_reload(cx));
        cx.run_until_parked();
        assert!(panel.read_with(cx, |p, _| p.state.is_none()), "no prior result means no auto-recompute");
    }

    /// `recompute_after_reload` re-runs against the pane's current
    /// bytes once a result already exists -- the reload cascade's
    /// entry point after `Workspace::resolve_reload` swaps the
    /// owning file's source.
    #[gpui::test]
    fn recompute_after_reload_reruns_when_a_result_exists(cx: &mut TestAppContext) {
        setup(cx);
        let len: u64 = 4096;
        let (panel, cx) = build(cx, vec![0xAAu8; len as usize]);
        cx.run_until_parked();
        assert!(panel.read_with(cx, |p, _| p.state.is_some()), "auto-ran on open");

        // Swap the owning pane's source directly (what
        // `apply_reload`/`swap_source` does under the hood) to a
        // buffer whose entropy differs, then confirm the recompute
        // picks up the new bytes rather than leaving the stale
        // result in place.
        let new_bytes: Vec<u8> = (0..len as usize).map(|i| i as u8).collect();
        let pane = panel.read_with(cx, |p, _| p.owning_pane.clone().unwrap());
        pane.update(cx, |pane, cx| {
            pane.editor_mut().swap_source(Arc::new(MemorySource::new(new_bytes)));
            cx.notify();
        });

        panel.update(cx, |p, cx| p.recompute_after_reload(cx));
        cx.run_until_parked();

        let mean = panel.read_with(cx, |p, _| p.state.as_ref().unwrap().mean());
        // Uniform 0xAA bytes have zero entropy per window; the
        // ramped replacement does not.
        assert!(mean > 0.0, "recompute picked up the new (non-uniform) bytes, mean={mean}");
    }
}
