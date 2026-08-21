//! [`VisualizerPanel`]: per-file dockable tab rendering the template
//! visualizers (`[[hex::visualize(...)]]` attributes).
//!
//! All decode/transform logic lives in `hxy_templates::visualize`
//! (shared with the egui front end); this module owns target
//! collection sync, fingerprinted texture/listing caches, and the
//! per-kind gpui bodies. Binding shape mirrors
//! [`EntropyPanel`](super::EntropyPanel): keyed by the owning file's
//! path, bound eagerly when opened from the palette / template-row
//! marker, or lazily via [`OpenFilePanels`](super::strings::OpenFilePanels)
//! when restored from a persisted layout. Unlike entropy, the panel
//! binds the whole [`FilePanel`] entity (not just its pane): targets
//! derive from the file's template instances, which live on the panel.
//!
//! Open/close semantics vs egui: egui keeps a per-file
//! `VisualizerPanel::open` flag and only auto-(re)opens the dock tab
//! after a run when that flag is set (`auto_open_visualizer_for`,
//! `crates/hxy/src/app/desktop.rs`). Here the dock tab's presence IS
//! that flag -- a persisted layout restores the tab, a template re-run
//! resyncs an already-open tab through the file observation, and a
//! file whose user never opened the panel stays quiet. No separate
//! auto-open pass is needed.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::AnyElement;
use gpui::App;
use gpui::AppContext;
use gpui::Bounds;
use gpui::Context;
use gpui::Entity;
use gpui::EventEmitter;
use gpui::FocusHandle;
use gpui::Focusable;
use gpui::IntoElement;
use gpui::ObjectFit;
use gpui::ParentElement;
use gpui::Pixels;
use gpui::Render;
use gpui::RenderImage;
use gpui::SharedString;
use gpui::Styled;
use gpui::StyledImage;
use gpui::Subscription;
use gpui::TextAlign;
use gpui::Window;
use gpui::bounds;
use gpui::canvas;
use gpui::div;
use gpui::fill;
use gpui::img;
use gpui::point;
use gpui::px;
use gpui::size;
use gpui::uniform_list;
use gpui_component::ActiveTheme;
use gpui_component::Icon;
use gpui_component::PixelsExt;
use gpui_component::Selectable;
use gpui_component::Sizable;
use gpui_component::button::Button;
use gpui_component::dock::Panel;
use gpui_component::dock::PanelEvent;
use gpui_component::dock::PanelInfo;
use gpui_component::dock::PanelState;
use gpui_component::h_flex;
use gpui_component::label::Label;
use gpui_component::plot::AXIS_GAP;
use gpui_component::plot::AxisText;
use gpui_component::plot::Grid;
use gpui_component::plot::IntoPlot;
use gpui_component::plot::Plot;
use gpui_component::plot::PlotAxis;
use gpui_component::plot::PlotLabel;
use gpui_component::plot::label::Text as PlotText;
use gpui_component::plot::scale::Scale;
use gpui_component::plot::scale::ScaleLinear;
use gpui_component::plot::shape::Bar;
use gpui_component::plot::shape::Line;
use gpui_component::table::Column;
use gpui_component::table::Table;
use gpui_component::table::TableDelegate;
use gpui_component::table::TableState;
use gpui_component::v_flex;
use hxy_core::format::TemplateValueFormats;
use hxy_core::format::format_offset;
use hxy_panels::entropy::MAX_ENTROPY;
use hxy_panels::entropy::pick_window_size;
use hxy_panels::entropy::shannon_entropy;
use hxy_plugin_host::template::Node;
use hxy_plugin_host::template::Value;
use hxy_templates::format::format_value;
use hxy_templates::visualize;
use hxy_templates::visualize::VisualizerKey;
use hxy_templates::visualize::VisualizerKind;
use hxy_templates::visualize::VisualizerSpec;
use hxy_templates::visualize::collect_targets;
use hxy_templates::visualize::read_field_bytes;

use super::FilePanel;
use super::strings::OpenFilePanels;
use crate::assets::HxyIcon;

/// Stable identifier for layout (de)serialization; must never change.
pub const VISUALIZER_PANEL_NAME: &str = "VisualizerPanel";

/// The currently published set of open file panels (empty before the
/// workspace publishes one). Same registry the strings/entropy rebind
/// paths read.
fn open_file_panels(cx: &App) -> Vec<Entity<FilePanel>> {
    match cx.try_global::<OpenFilePanels>() {
        Some(g) => g.0.clone(),
        None => Vec::new(),
    }
}

/// One sub-tab strip entry, precomputed at sync so render doesn't
/// re-walk the template trees.
struct TargetEntry {
    key: VisualizerKey,
    spec: VisualizerSpec,
    label: String,
    byte_offset: u64,
    byte_length: u64,
}

/// Decoded-texture cache slot: rebuilt only when the content
/// fingerprint moves, so a template re-run over unchanged bytes keeps
/// the same GPU texture (egui `ImageCache`/`BitmapCache` parity).
struct TextureCache {
    fingerprint: [u8; 32],
    texture: Option<Arc<RenderImage>>,
    size: (u32, u32),
    error: Option<String>,
}

/// Disassembly listing cache: the listing can be tens of kB, so it is
/// decoded once per fingerprint and pre-split for `uniform_list`.
struct DisasmCache {
    fingerprint: [u8; 32],
    lines: Arc<Vec<SharedString>>,
    instruction_count: usize,
    error: Option<String>,
}

/// Downsampled waveform cache (sound kind).
struct SoundPlotCache {
    fingerprint: [u8; 32],
    samples: Vec<f64>,
}

/// Per-target renderer caches, keyed by [`VisualizerKey`] on the
/// panel. Dropped by [`VisualizerPanel::sync_from_file`]'s gc when a
/// re-run renumbers or removes the backing node (egui
/// `VisualizerPanel::gc` parity).
#[derive(Default)]
struct KindCache {
    image: Option<TextureCache>,
    bitmap: Option<TextureCache>,
    digram: Option<TextureCache>,
    distribution: Option<TextureCache>,
    sound: Option<SoundPlotCache>,
    disasm: Option<DisasmCache>,
}

/// One formatted row of the table kind's grid, precomputed at sync so
/// the table delegate renders plain strings.
#[derive(Clone)]
struct VisTableRow {
    name: SharedString,
    type_label: SharedString,
    offset: SharedString,
    length: SharedString,
    value: SharedString,
}

pub struct VisualizerPanel {
    focus_handle: FocusHandle,
    owning_path: Option<PathBuf>,
    owning_file: Option<Entity<FilePanel>>,
    _rebind_observe: Subscription,
    /// Rebuilds the precomputed table rows (whose offset/length/value
    /// cells bake in the settings formats) when settings change.
    _settings_observe: Subscription,
    /// Re-syncs targets whenever the bound file panel notifies (run
    /// completion, re-run, instance removal, byte edits).
    _file_observe: Option<Subscription>,
    targets: Vec<TargetEntry>,
    /// The sub-tab the user last selected; normalized to the first
    /// target whenever the selection's node disappears (egui parity).
    active: Option<VisualizerKey>,
    caches: HashMap<VisualizerKey, KindCache>,
    /// Rows for the table kind's grid; empty unless the active target
    /// is `VisualizerKind::Table`.
    table_rows: Vec<VisTableRow>,
    table: Entity<TableState<VisTableDelegate>>,
}

impl VisualizerPanel {
    /// Build a fresh panel bound immediately to `file` (the palette /
    /// template-row open path, where the owning file is already live).
    pub fn new(file: Entity<FilePanel>, path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self::with_state(path, window, cx);
        this.bind_file(file, cx);
        this
    }

    /// Rebuild from persisted [`PanelInfo`]; binds lazily via
    /// [`OpenFilePanels`] (the owning file may not be restored yet).
    pub fn restore(info: &PanelInfo, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path = path_from_info(info);
        let mut this = Self::with_state(path, window, cx);
        this.try_bind_from_global(cx);
        this
    }

    fn with_state(path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let rebind_observe = cx.observe_global::<OpenFilePanels>(|this, cx| this.try_bind_from_global(cx));
        let settings_observe = cx.observe_global::<crate::settings::SettingsGlobal>(|this, cx| this.sync_from_file(cx));
        let weak = cx.entity().downgrade();
        let table = cx.new(|cx| TableState::new(VisTableDelegate::new(weak), window, cx));
        Self {
            focus_handle: cx.focus_handle(),
            owning_path: path,
            owning_file: None,
            _rebind_observe: rebind_observe,
            _settings_observe: settings_observe,
            _file_observe: None,
            targets: Vec::new(),
            active: None,
            caches: HashMap::new(),
            table_rows: Vec::new(),
            table,
        }
    }

    /// The file path this panel visualizes, for the workspace's
    /// open-tab dedup and dock persistence.
    pub(crate) fn owning_path(&self) -> Option<&Path> {
        self.owning_path.as_deref()
    }

    /// Re-anchor onto a new owning path (Save As renamed the file).
    pub(crate) fn set_owning_path(&mut self, path: PathBuf) {
        self.owning_path = Some(path);
    }

    fn try_bind_from_global(&mut self, cx: &mut Context<Self>) {
        if self.owning_file.is_some() {
            return;
        }
        let Some(path) = self.owning_path.clone() else { return };
        let Some(file) = open_file_panels(cx).into_iter().find(|f| f.read(cx).path() == Some(path.as_path())) else {
            return;
        };
        self.bind_file(file, cx);
    }

    fn bind_file(&mut self, file: Entity<FilePanel>, cx: &mut Context<Self>) {
        if self.owning_file.is_some() {
            return;
        }
        self._file_observe = Some(cx.observe(&file, |this, _file, cx| this.sync_from_file(cx)));
        self.owning_file = Some(file);
        self.sync_from_file(cx);
    }

    /// Select `key`'s sub-tab. A key no longer backed by a live target
    /// falls back to the first target during the sync below.
    pub(crate) fn set_active(&mut self, key: VisualizerKey, cx: &mut Context<Self>) {
        self.active = Some(key);
        self.sync_from_file(cx);
    }

    #[cfg(test)]
    pub(crate) fn active_key(&self) -> Option<VisualizerKey> {
        self.active
    }

    #[cfg(test)]
    pub(crate) fn owning_file_for_test(&self) -> Option<Entity<FilePanel>> {
        self.owning_file.clone()
    }

    #[cfg(test)]
    pub(crate) fn target_count(&self) -> usize {
        self.targets.len()
    }

    #[cfg(test)]
    pub(crate) fn cached_keys(&self) -> Vec<VisualizerKey> {
        self.caches.keys().copied().collect()
    }

    #[cfg(test)]
    pub(crate) fn insert_cache_for_test(&mut self, key: VisualizerKey) {
        self.caches.insert(key, KindCache::default());
    }

    /// The formatted (offset, length) cell texts of the table rows.
    #[cfg(test)]
    pub(crate) fn table_row_spans_for_test(&self) -> Vec<(String, String)> {
        self.table_rows.iter().map(|r| (r.offset.to_string(), r.length.to_string())).collect()
    }

    /// Re-derive the target list from the bound file's template
    /// instances, gc caches whose node is gone, normalize the active
    /// sub-tab, and rebuild the table rows. The single sync point for
    /// bind, file notify (run completion / re-run / removal), and
    /// sub-tab selection.
    fn sync_from_file(&mut self, cx: &mut Context<Self>) {
        let Some(file) = self.owning_file.clone() else { return };
        let targets: Vec<TargetEntry> = collect_targets(&file.read(cx).templates)
            .into_iter()
            .map(|t| {
                let label = format!("{} ({})", t.label, t.spec.kind.label());
                TargetEntry { key: t.key, spec: t.spec, label, byte_offset: t.byte_offset, byte_length: t.byte_length }
            })
            .collect();
        let live: HashSet<VisualizerKey> = targets.iter().map(|t| t.key).collect();
        self.caches.retain(|k, _| live.contains(k));
        if !self.active.is_some_and(|a| live.contains(&a)) {
            self.active = targets.first().map(|t| t.key);
        }
        self.targets = targets;
        self.sync_table_rows(cx);
        cx.notify();
    }

    /// Rebuild the table kind's rows: the active target's direct
    /// children, formatted with the same helpers the template panel
    /// uses (`format_offset` / `format_value`; `BytesVal` collapses to
    /// the compact `[N bytes]` summary, egui `table.rs` parity).
    fn sync_table_rows(&mut self, cx: &mut Context<Self>) {
        let rows = self.build_table_rows(cx);
        self.table_rows = rows;
        self.table.update(cx, |table, cx| table.refresh(cx));
    }

    fn build_table_rows(&self, cx: &App) -> Vec<VisTableRow> {
        let Some(file) = &self.owning_file else { return Vec::new() };
        let Some(key) = self.active else { return Vec::new() };
        let Some(target) = self.targets.iter().find(|t| t.key == key) else { return Vec::new() };
        if target.spec.kind != VisualizerKind::Table {
            return Vec::new();
        }
        let file = file.read(cx);
        let Some(instance) = file.templates.iter().find(|t| t.id == key.instance) else { return Vec::new() };
        let parent = key.node.0;
        // User formats from the settings global (replaces the M4a
        // `Default::default()` placeholders).
        let (numeric_format, value_formats) = crate::settings::formats(cx);
        let format_num = |value: u64| format_offset(value, numeric_format.pick(value));
        instance
            .state
            .tree
            .nodes
            .iter()
            .filter(|n| n.parent == Some(parent))
            .map(|n| VisTableRow {
                name: SharedString::from(n.name.clone()),
                type_label: SharedString::from(hxy_plugin_host::node_display_type(n)),
                offset: SharedString::from(format_num(n.span.offset)),
                length: SharedString::from(format_num(n.span.length)),
                value: SharedString::from(format_table_value(n, &value_formats)),
            })
            .collect()
    }

    fn render_tab_strip(&self, cx: &Context<Self>) -> impl IntoElement {
        let mut strip = h_flex().gap_1().px_2().py_1().flex_wrap().items_center();
        for (i, target) in self.targets.iter().enumerate() {
            let key = target.key;
            strip = strip.child(
                Button::new(("vis-tab", i))
                    .compact()
                    .label(target.label.clone())
                    .selected(Some(key) == self.active)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_active(key, cx))),
            );
        }
        strip
    }

    fn muted_line(&self, text: String, cx: &Context<Self>) -> AnyElement {
        div().p_2().text_color(cx.theme().muted_foreground).child(text).into_any_element()
    }

    fn error_line(&self, text: String, cx: &Context<Self>) -> AnyElement {
        div().p_2().text_color(cx.theme().danger).child(text).into_any_element()
    }

    fn info_label(&self, text: String, cx: &Context<Self>) -> AnyElement {
        div().px_2().pt_1().text_sm().text_color(cx.theme().muted_foreground).child(text).into_any_element()
    }

    fn warn_label(&self, text: String, cx: &Context<Self>) -> AnyElement {
        div().px_2().text_sm().text_color(cx.theme().warning).child(text).into_any_element()
    }

    /// The active target's body. Reads the field's bytes fresh each
    /// render (renders are event-driven here, unlike egui's per-frame
    /// show) and dispatches on the visualizer kind.
    fn render_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(file) = self.owning_file.clone() else {
            return self.muted_line(hxy_i18n::t("visualizer-no-file"), cx);
        };
        let Some(key) = self.active else {
            return self.muted_line(hxy_i18n::t("visualizer-no-targets"), cx);
        };
        let Some(target) = self.targets.iter().find(|t| t.key == key) else {
            return self.muted_line(hxy_i18n::t("visualizer-no-targets"), cx);
        };
        let spec = target.spec.clone();
        let byte_offset = target.byte_offset;
        let byte_length = target.byte_length;

        let source = file.read(cx).pane().read(cx).editor().source().clone();
        let bytes = match read_field_bytes(&source, byte_offset, byte_length) {
            Ok(bytes) => bytes,
            Err(err) => return self.error_line(err, cx),
        };

        match &spec.kind {
            VisualizerKind::Image => self.render_image(key, &bytes, cx),
            VisualizerKind::Bitmap => self.render_bitmap(key, &spec, &bytes, cx),
            VisualizerKind::Digram => self.render_digram(key, &bytes, cx),
            VisualizerKind::LayeredDistribution => self.render_distribution(key, &bytes, cx),
            VisualizerKind::HexViewer => self.render_hex_viewer(byte_offset, &bytes, cx),
            VisualizerKind::Text => self.render_text(&spec, &bytes, cx),
            VisualizerKind::ChunkEntropy => self.render_chunk_entropy(&spec, &bytes, cx),
            VisualizerKind::LinePlot => self.render_series(&spec, &bytes, SeriesShape::Line, cx),
            VisualizerKind::BarChart => self.render_series(&spec, &bytes, SeriesShape::Bar, cx),
            VisualizerKind::ScatterPlot => self.render_scatter(&spec, &bytes, cx),
            VisualizerKind::Sound => self.render_sound(key, &spec, &bytes, cx),
            VisualizerKind::Disassembler => self.render_disasm(key, &spec, byte_offset, &bytes, cx),
            VisualizerKind::Coordinates => self.render_coordinates(&spec, &bytes, cx),
            VisualizerKind::Timestamp => self.render_timestamp(&spec, &bytes, cx),
            VisualizerKind::Table => self.render_table(cx),
            VisualizerKind::ThreeD => self.render_three_d(&bytes, cx),
            VisualizerKind::Unknown(name) => {
                self.muted_line(hxy_i18n::t_args("visualizer-unknown", &[("name", name)]), cx)
            }
        }
    }

    fn render_image(&mut self, key: VisualizerKey, bytes: &[u8], cx: &mut Context<Self>) -> AnyElement {
        let fingerprint = visualize::image::fingerprint(bytes);
        let cache = self.caches.entry(key).or_default();
        let slot = ensure_texture(&mut cache.image, fingerprint, || visualize::image::decode_rgba(bytes));
        if let Some(err) = slot.error.clone() {
            let text = hxy_i18n::t_args("visualizer-image-decode-failed", &[("error", &err)]);
            return self.error_line(text, cx);
        }
        let Some(texture) = slot.texture.clone() else {
            return self.muted_line(hxy_i18n::t("visualizer-image-empty"), cx);
        };
        let (w, h) = slot.size;
        let info = hxy_i18n::t_args(
            "visualizer-image-info",
            &[("w", &w.to_string()), ("h", &h.to_string()), ("bytes", &bytes.len().to_string())],
        );
        v_flex()
            .size_full()
            .child(self.info_label(info, cx))
            .child(div().flex_1().min_h_0().p_2().child(texture_img(texture, ObjectFit::ScaleDown)))
            .into_any_element()
    }

    fn render_bitmap(
        &mut self,
        key: VisualizerKey,
        spec: &VisualizerSpec,
        bytes: &[u8],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let fingerprint = visualize::bitmap::blake3_short_with_args(bytes, &spec.args);
        let cache = self.caches.entry(key).or_default();
        let slot = ensure_texture(&mut cache.bitmap, fingerprint, || visualize::bitmap::decode(bytes, &spec.args));
        if let Some(err) = slot.error.clone() {
            return self.error_line(err, cx);
        }
        let Some(texture) = slot.texture.clone() else {
            return self.muted_line(hxy_i18n::t("visualizer-bitmap-empty"), cx);
        };
        let (w, h) = slot.size;
        let info = hxy_i18n::t_args("visualizer-bitmap-info", &[("w", &w.to_string()), ("h", &h.to_string())]);
        v_flex()
            .size_full()
            .child(self.info_label(info, cx))
            .child(div().flex_1().min_h_0().p_2().child(texture_img(texture, ObjectFit::ScaleDown)))
            .into_any_element()
    }

    fn render_digram(&mut self, key: VisualizerKey, bytes: &[u8], cx: &mut Context<Self>) -> AnyElement {
        let fingerprint = visualize::image::fingerprint(bytes);
        let cache = self.caches.entry(key).or_default();
        let slot = ensure_texture(&mut cache.digram, fingerprint, || {
            let side = visualize::digram::SIDE as u32;
            match visualize::digram::build_grid(bytes) {
                Some(grid) => Ok((side, side, rgba_grid_bytes(&grid))),
                None => Err(hxy_i18n::t("visualizer-digram-empty")),
            }
        });
        if let Some(err) = slot.error.clone() {
            return self.muted_line(err, cx);
        }
        let Some(texture) = slot.texture.clone() else {
            return self.muted_line(hxy_i18n::t("visualizer-digram-empty"), cx);
        };
        let info = hxy_i18n::t_args("visualizer-digram-info", &[("pairs", &bytes.len().saturating_sub(1).to_string())]);
        v_flex()
            .size_full()
            .child(self.info_label(info, cx))
            .child(div().flex_1().min_h_0().p_2().child(texture_img(texture, ObjectFit::Contain)))
            .into_any_element()
    }

    fn render_distribution(&mut self, key: VisualizerKey, bytes: &[u8], cx: &mut Context<Self>) -> AnyElement {
        let fingerprint = visualize::image::fingerprint(bytes);
        let cache = self.caches.entry(key).or_default();
        let slot =
            ensure_texture(&mut cache.distribution, fingerprint, || match visualize::distribution::build_grid(bytes) {
                Some((cols, grid)) => Ok((cols as u32, visualize::distribution::HEIGHT as u32, rgba_grid_bytes(&grid))),
                None => Err(hxy_i18n::t("visualizer-distribution-empty")),
            });
        if let Some(err) = slot.error.clone() {
            return self.muted_line(err, cx);
        }
        let Some(texture) = slot.texture.clone() else {
            return self.muted_line(hxy_i18n::t("visualizer-distribution-empty"), cx);
        };
        let (cols, _) = slot.size;
        let info = hxy_i18n::t_args(
            "visualizer-distribution-info",
            &[("bytes", &bytes.len().to_string()), ("cols", &cols.to_string())],
        );
        v_flex()
            .size_full()
            .child(self.info_label(info, cx))
            .child(div().flex_1().min_h_0().p_2().child(texture_img(texture, ObjectFit::Contain)))
            .into_any_element()
    }

    fn render_hex_viewer(&self, byte_offset: u64, bytes: &[u8], cx: &Context<Self>) -> AnyElement {
        let truncated = bytes.len() > visualize::hex_dump::MAX_BYTES;
        let view = &bytes[..bytes.len().min(visualize::hex_dump::MAX_BYTES)];
        let dump = visualize::hex_dump::format_dump(byte_offset, view);
        let lines: Arc<Vec<SharedString>> = Arc::new(dump.lines().map(|l| SharedString::from(l.to_owned())).collect());
        let info = hxy_i18n::t_args(
            "visualizer-hex-info",
            &[("offset", &format!("{byte_offset:#x}")), ("len", &bytes.len().to_string())],
        );
        let mut root = v_flex().size_full().child(self.info_label(info, cx));
        if truncated {
            let text =
                hxy_i18n::t_args("visualizer-hex-truncated", &[("max", &visualize::hex_dump::MAX_BYTES.to_string())]);
            root = root.child(self.warn_label(text, cx));
        }
        root.child(mono_list("vis-hex-dump", lines, cx)).into_any_element()
    }

    fn render_text(&self, spec: &VisualizerSpec, bytes: &[u8], cx: &Context<Self>) -> AnyElement {
        let encoding = spec.args.first().map(|s| s.as_str()).unwrap_or("utf-8").to_ascii_lowercase();
        let truncated = bytes.len() > visualize::text::MAX_BYTES;
        let view = &bytes[..bytes.len().min(visualize::text::MAX_BYTES)];
        let decoded = match visualize::text::decode(view, &encoding) {
            Ok(d) => d,
            Err(e) => return self.error_line(e, cx),
        };
        let lines: Arc<Vec<SharedString>> =
            Arc::new(decoded.lines().map(|l| SharedString::from(l.to_owned())).collect());
        let info =
            hxy_i18n::t_args("visualizer-text-info", &[("encoding", &encoding), ("bytes", &bytes.len().to_string())]);
        let mut root = v_flex().size_full().child(self.info_label(info, cx));
        if truncated {
            let text =
                hxy_i18n::t_args("visualizer-text-truncated", &[("max", &visualize::text::MAX_BYTES.to_string())]);
            root = root.child(self.warn_label(text, cx));
        }
        root.child(mono_list("vis-text", lines, cx)).into_any_element()
    }

    /// Per-chunk Shannon entropy over the field, fixed Y domain
    /// `[0, MAX_ENTROPY]` (egui parity: `show_chunk_entropy` pins the
    /// plot bounds). DEVIATION: X ticks are hex offsets with no axis
    /// captions, matching this shell's EntropyPanel chart rather than
    /// egui's decimal ticks + `visualizer-entropy-x`/`-y` labels.
    fn render_chunk_entropy(&self, spec: &VisualizerSpec, bytes: &[u8], cx: &Context<Self>) -> AnyElement {
        let window: u64 = spec
            .args
            .first()
            .and_then(|a| a.parse().ok())
            .unwrap_or_else(|| pick_window_size(bytes.len() as u64))
            .max(1);
        let mut points: Vec<(f64, f64)> = Vec::new();
        let mut offset: usize = 0;
        while offset < bytes.len() {
            let end = (offset + window as usize).min(bytes.len());
            let h = shannon_entropy(&bytes[offset..end]);
            points.push((offset as f64 + window as f64 / 2.0, h));
            offset = end;
        }
        if points.is_empty() {
            return self.muted_line(hxy_i18n::t("visualizer-plot-no-samples"), cx);
        }
        let max_x = (bytes.len() as f64).max(1.0);
        chart_element(VisualizerChart {
            series: ChartSeries::Line(points),
            x_domain: (0.0, max_x),
            y_domain: (0.0, MAX_ENTROPY),
            x_labels_hex: true,
        })
    }

    fn render_series(&self, spec: &VisualizerSpec, bytes: &[u8], shape: SeriesShape, cx: &Context<Self>) -> AnyElement {
        let sample = visualize::plot::parse_sample(&spec.args);
        let values = visualize::plot::samples(bytes, sample);
        if values.is_empty() {
            return self.muted_line(hxy_i18n::t("visualizer-plot-no-samples"), cx);
        }
        let points: Vec<(f64, f64)> = values.iter().enumerate().map(|(i, v)| (i as f64, *v)).collect();
        let (mut y0, mut y1) = value_extent(values.iter().copied());
        if shape == SeriesShape::Bar {
            // Bars baseline at zero, like egui_plot's BarChart. The
            // first and last bars extend band/2 past the x domain and
            // clip at the chart edge; egui_plot auto-expands bounds.
            y0 = y0.min(0.0);
            y1 = y1.max(0.0);
        }
        let series = match shape {
            SeriesShape::Line => ChartSeries::Line(points),
            SeriesShape::Bar => ChartSeries::Bar(points),
        };
        chart_element(VisualizerChart {
            series,
            x_domain: (0.0, (values.len().saturating_sub(1) as f64).max(1.0)),
            y_domain: (y0, y1),
            x_labels_hex: false,
        })
    }

    fn render_scatter(&self, spec: &VisualizerSpec, bytes: &[u8], cx: &Context<Self>) -> AnyElement {
        let sample = visualize::plot::parse_sample(&spec.args);
        let values = visualize::plot::samples(bytes, sample);
        if values.len() < 2 {
            return self.muted_line(hxy_i18n::t("visualizer-plot-no-samples"), cx);
        }
        let points: Vec<(f64, f64)> = values.chunks_exact(2).map(|c| (c[0], c[1])).collect();
        let (x0, x1) = value_extent(points.iter().map(|p| p.0));
        let (y0, y1) = value_extent(points.iter().map(|p| p.1));
        chart_element(VisualizerChart {
            series: ChartSeries::Scatter(points),
            x_domain: (x0, x1),
            y_domain: (y0, y1),
            x_labels_hex: false,
        })
    }

    fn render_sound(
        &mut self,
        key: VisualizerKey,
        spec: &VisualizerSpec,
        bytes: &[u8],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let channels: u16 = spec.args.first().and_then(|a| a.parse().ok()).unwrap_or(1);
        let sample_rate: u32 = spec.args.get(1).and_then(|a| a.parse().ok()).unwrap_or(44_100);
        let format = spec
            .args
            .get(2)
            .and_then(|s| visualize::sound::SampleFormat::parse(s))
            .unwrap_or(visualize::sound::SampleFormat::PcmS16Le);
        let fingerprint = visualize::image::fingerprint(bytes);
        let cache = self.caches.entry(key).or_default();
        let stale = cache.sound.as_ref().map(|c| c.fingerprint != fingerprint).unwrap_or(true);
        if stale {
            cache.sound = Some(SoundPlotCache {
                fingerprint,
                samples: visualize::sound::downsample_for_plot(bytes, format, channels),
            });
        }
        // Extract the plot inputs inside one scope so the cache
        // borrow ends before the &self element helpers below.
        let (points, y0, y1, sample_count) = {
            let samples = &cache.sound.as_ref().expect("filled above").samples;
            let (y0, y1) = value_extent(samples.iter().copied());
            let points: Vec<(f64, f64)> = samples.iter().enumerate().map(|(i, v)| (i as f64, *v)).collect();
            (points, y0, y1, samples.len())
        };
        if sample_count == 0 {
            return self.muted_line(hxy_i18n::t("visualizer-sound-empty"), cx);
        }
        let duration_secs = sample_count as f64 / sample_rate.max(1) as f64;
        let info = hxy_i18n::t_args(
            "visualizer-sound-info",
            &[
                ("ch", &channels.to_string()),
                ("rate", &sample_rate.to_string()),
                ("seconds", &format!("{duration_secs:.2}")),
            ],
        );
        let chart = chart_element(VisualizerChart {
            series: ChartSeries::Line(points),
            x_domain: (0.0, (sample_count.saturating_sub(1) as f64).max(1.0)),
            y_domain: (y0, y1),
            x_labels_hex: false,
        });
        v_flex()
            .size_full()
            .child(self.info_label(info, cx))
            .child(self.warn_label(hxy_i18n::t("visualizer-sound-no-playback"), cx))
            .child(div().flex_1().min_h_0().p_2().child(chart))
            .into_any_element()
    }

    fn render_disasm(
        &mut self,
        key: VisualizerKey,
        spec: &VisualizerSpec,
        byte_offset: u64,
        bytes: &[u8],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let fingerprint = visualize::disasm::blake3_with_args(bytes, &spec.args);
        // Base defaults to the field's file offset, ISA to x86-64
        // (egui `disassembler.rs` parity).
        let base_address: u64 = spec.args.first().and_then(|a| visualize::disasm::parse_addr(a)).unwrap_or(byte_offset);
        let isa = spec.args.get(1).map(|s| s.as_str()).unwrap_or("x86-64").to_owned();
        let cache = self.caches.entry(key).or_default();
        let stale = cache.disasm.as_ref().map(|c| c.fingerprint != fingerprint).unwrap_or(true);
        if stale {
            cache.disasm = Some(match visualize::disasm::Bitness::parse(&isa) {
                Some(bitness) => {
                    let mut decoded = visualize::disasm::DisassemblerCache::default();
                    visualize::disasm::disassemble_x86(bytes, bitness, base_address, &mut decoded);
                    DisasmCache {
                        fingerprint,
                        lines: Arc::new(decoded.listing.lines().map(|l| SharedString::from(l.to_owned())).collect()),
                        instruction_count: decoded.instruction_count,
                        error: None,
                    }
                }
                None => DisasmCache {
                    fingerprint,
                    lines: Arc::new(Vec::new()),
                    instruction_count: 0,
                    error: Some(hxy_i18n::t_args("visualizer-disasm-unsupported-isa", &[("isa", &isa)])),
                },
            });
        }
        let Some(slot) = &cache.disasm else {
            return self.muted_line(hxy_i18n::t("visualizer-plot-no-samples"), cx);
        };
        if let Some(err) = slot.error.clone() {
            return self.error_line(err, cx);
        }
        let lines = slot.lines.clone();
        let info = hxy_i18n::t_args(
            "visualizer-disasm-info",
            &[("isa", &isa), ("base", &format!("{base_address:#x}")), ("count", &slot.instruction_count.to_string())],
        );
        v_flex()
            .size_full()
            .child(self.info_label(info, cx))
            .child(mono_list("vis-disasm", lines, cx))
            .into_any_element()
    }

    fn render_coordinates(&self, spec: &VisualizerSpec, bytes: &[u8], cx: &Context<Self>) -> AnyElement {
        let (lat, lng) = match visualize::coordinates::resolve_coordinates(bytes, &spec.args) {
            Ok(v) => v,
            Err(e) => return self.error_line(e, cx),
        };
        let info =
            hxy_i18n::t_args("visualizer-coords-info", &[("lat", &format!("{lat:.6}")), ("lng", &format!("{lng:.6}"))]);
        let bg = cx.theme().secondary;
        let grid = cx.theme().border;
        let dot = cx.theme().primary;
        let world = canvas(
            move |_bounds, _window, _cx| {},
            move |bounds, _prepaint, window, _cx| paint_world(bounds, lat, lng, bg, grid, dot, window),
        )
        .w_full()
        .h_full();
        v_flex()
            .size_full()
            .child(div().px_2().pt_1().child(Label::new(info)))
            .child(div().flex_1().min_h_0().max_w(px(640.0)).p_2().child(world))
            .into_any_element()
    }

    fn render_timestamp(&self, spec: &VisualizerSpec, bytes: &[u8], cx: &Context<Self>) -> AnyElement {
        let format = spec
            .args
            .first()
            .map(|s| s.as_str())
            .unwrap_or_else(|| if bytes.len() >= 8 { "unix64" } else { "unix" })
            .to_owned();
        match visualize::timestamp::decode(bytes, &format) {
            // Both lines render jiff's RFC 3339 Display today; egui
            // keeps the second (machine-readable) line separate so a
            // locale-aware first line can land later without touching
            // it. Same rule here.
            Ok(ts) => v_flex()
                .gap_1()
                .child(self.info_label(hxy_i18n::t_args("visualizer-timestamp-info", &[("format", &format)]), cx))
                .child(
                    div()
                        .px_2()
                        .font_family(cx.theme().mono_font_family.clone())
                        .font_weight(gpui::FontWeight::BOLD)
                        .child(format!("{ts}")),
                )
                .child(div().px_2().font_family(cx.theme().mono_font_family.clone()).child(format!("{ts}")))
                .into_any_element(),
            Err(e) => self.error_line(e, cx),
        }
    }

    fn render_table(&self, cx: &Context<Self>) -> AnyElement {
        if self.table_rows.is_empty() {
            return self.muted_line(hxy_i18n::t("visualizer-table-no-children"), cx);
        }
        let info = hxy_i18n::t_args("visualizer-table-info", &[("count", &self.table_rows.len().to_string())]);
        v_flex()
            .size_full()
            .child(self.info_label(info, cx))
            .child(div().flex_1().min_h_0().p_2().child(Table::new(&self.table)))
            .into_any_element()
    }

    fn render_three_d(&self, bytes: &[u8], cx: &Context<Self>) -> AnyElement {
        v_flex()
            .gap_1()
            .child(div().px_2().pt_1().child(Label::new(hxy_i18n::t("visualizer-3d-heading"))))
            .child(self.info_label(hxy_i18n::t_args("visualizer-3d-info", &[("bytes", &bytes.len().to_string())]), cx))
            .child(self.warn_label(hxy_i18n::t("visualizer-3d-not-yet"), cx))
            .into_any_element()
    }
}

/// Which numeric series shape a plot kind draws.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SeriesShape {
    Line,
    Bar,
}

/// Extract the stored owning path from a `VisualizerPanel` payload.
fn path_from_info(info: &PanelInfo) -> Option<PathBuf> {
    let PanelInfo::Panel(value) = info else { return None };
    value.get("path").and_then(|p| p.as_str()).map(PathBuf::from)
}

/// The panel base name shown on its tab (owning file's leaf name).
fn tab_label(path: Option<&Path>) -> String {
    match path {
        Some(path) => path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
        None => hxy_i18n::t("gpui-file-untitled"),
    }
}

/// Flatten a decoded color grid into tightly packed RGBA bytes.
fn rgba_grid_bytes(grid: &[hxy_core::color::Rgba]) -> Vec<u8> {
    let mut out = Vec::with_capacity(grid.len() * 4);
    for p in grid {
        out.extend_from_slice(&[p.r, p.g, p.b, p.a]);
    }
    out
}

/// Build a texture from a tightly packed RGBA buffer.
/// [`RenderImage`] stores BGRA byte order (see `gpui::RenderImage`'s
/// doc; gpui's own image loader swaps channels the same way), so the
/// red and blue channels are swapped in place first.
fn render_image_from_rgba(w: u32, h: u32, mut rgba: Vec<u8>) -> Arc<RenderImage> {
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    // Decoders emit exactly w*h*4 bytes (their contract); a mismatch
    // is a decoder bug, not user input.
    let buffer = image::RgbaImage::from_raw(w, h, rgba).expect("decoders emit w*h*4 RGBA bytes");
    Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]))
}

/// Fill (or reuse) a fingerprinted texture slot. The decode closure
/// only runs when the fingerprint moved, so unchanged bytes across a
/// template re-run keep their texture.
fn ensure_texture(
    slot: &mut Option<TextureCache>,
    fingerprint: [u8; 32],
    decode: impl FnOnce() -> Result<(u32, u32, Vec<u8>), String>,
) -> &TextureCache {
    let stale = slot.as_ref().map(|c| c.fingerprint != fingerprint).unwrap_or(true);
    if stale {
        *slot = Some(match decode() {
            Ok((w, h, rgba)) => TextureCache {
                fingerprint,
                texture: Some(render_image_from_rgba(w, h, rgba)),
                size: (w, h),
                error: None,
            },
            Err(error) => TextureCache { fingerprint, texture: None, size: (0, 0), error: Some(error) },
        });
    }
    slot.as_ref().expect("filled above")
}

/// The shared texture-kind body. `fit` mirrors egui's per-kind
/// scaling: image/bitmap fit-to-width but never upscale
/// (`ScaleDown`), digram/distribution scale to fill the panel,
/// aspect preserved, upscaling included (`Contain` -- egui's
/// `avail.min(...)` sizing). DEVIATION: gpui 0.2.2's `img()` has no
/// scroll-at-native-size affordance and no sampler control (the
/// grid/bitmap kinds render with the default bilinear filter, not
/// egui's NEAREST).
fn texture_img(texture: Arc<RenderImage>, fit: ObjectFit) -> impl IntoElement {
    img(texture).object_fit(fit).size_full()
}

/// Virtualized monospace line list for the text / hex dump /
/// disassembly kinds. Long lines are clipped, not h-scrolled
/// (uniform_list is vertical-only).
fn mono_list(id: &'static str, lines: Arc<Vec<SharedString>>, cx: &App) -> AnyElement {
    let count = lines.len();
    let mono = cx.theme().mono_font_family.clone();
    uniform_list(id, count, move |range, _window, _cx| {
        range
            .map(|i| {
                div()
                    .px_2()
                    .font_family(mono.clone())
                    .text_sm()
                    .whitespace_nowrap()
                    .child(lines[i].clone())
                    .into_any_element()
            })
            .collect::<Vec<_>>()
    })
    .flex_1()
    .p_1()
    .into_any_element()
}

/// Min/max of a value stream, widened when degenerate so the linear
/// scales never see a zero-span domain.
fn value_extent(values: impl Iterator<Item = f64>) -> (f64, f64) {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for v in values {
        if v.is_finite() {
            min = min.min(v);
            max = max.max(v);
        }
    }
    if !min.is_finite() || !max.is_finite() {
        return (0.0, 1.0);
    }
    if min == max {
        return (min - 0.5, max + 0.5);
    }
    (min, max)
}

/// Numeric series drawn by [`VisualizerChart`].
enum ChartSeries {
    Line(Vec<(f64, f64)>),
    Bar(Vec<(f64, f64)>),
    /// Painted as small filled quads with an equal-aspect domain fit.
    Scatter(Vec<(f64, f64)>),
}

/// Left margin reserved for Y-axis value labels.
const Y_LABEL_MARGIN: f32 = 40.0;
const X_TICKS: usize = 6;
const Y_TICKS: usize = 4;

fn chart_element(chart: VisualizerChart) -> AnyElement {
    div().size_full().p_2().child(chart).into_any_element()
}

/// One plot body assembled from gpui-component's plot primitives, the
/// same way [`EntropyPanel`](super::entropy)'s chart is (see that
/// module's doc for why the packaged `LineChart` doesn't fit: it
/// auto-fits Y and gives every point an X label slot).
#[derive(IntoPlot)]
struct VisualizerChart {
    series: ChartSeries,
    x_domain: (f64, f64),
    y_domain: (f64, f64),
    /// Hex-format the X tick labels (chunk entropy's file offsets).
    x_labels_hex: bool,
}

/// Short tick label: plain integers when small, scientific otherwise.
fn format_tick(v: f64) -> String {
    let a = v.abs();
    if a >= 1e6 || (a > 0.0 && a < 1e-3) {
        format!("{v:.1e}")
    } else if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.2}")
    }
}

impl Plot for VisualizerChart {
    fn paint(&mut self, plot_bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let width = plot_bounds.size.width.as_f32();
        let height = plot_bounds.size.height.as_f32() - AXIS_GAP;
        if width <= Y_LABEL_MARGIN || height <= 0.0 {
            return;
        }
        let chart_bounds = bounds(
            plot_bounds.origin + point(px(Y_LABEL_MARGIN), px(0.)),
            size(px(width - Y_LABEL_MARGIN), plot_bounds.size.height),
        );
        let plot_width = width - Y_LABEL_MARGIN;

        let (mut x0, mut x1) = self.x_domain;
        let (mut y0, mut y1) = self.y_domain;
        // The Y scale maps onto [height, 10] below, so its pixel run
        // is 10 short of the full chart height.
        let y_px_run = (height - 10.0).max(1.0);
        if let ChartSeries::Scatter(_) = self.series {
            // Equal aspect: widen the sparser axis's domain (centered)
            // so one data unit spans the same pixel run on X and Y --
            // egui parity's `data_aspect(1.0)`.
            let x_per_px = (x1 - x0) / plot_width.max(1.0) as f64;
            let y_per_px = (y1 - y0) / y_px_run as f64;
            let per_px = x_per_px.max(y_per_px);
            let x_mid = (x0 + x1) / 2.0;
            let y_mid = (y0 + y1) / 2.0;
            let x_half = per_px * plot_width as f64 / 2.0;
            let y_half = per_px * y_px_run as f64 / 2.0;
            (x0, x1) = (x_mid - x_half, x_mid + x_half);
            (y0, y1) = (y_mid - y_half, y_mid + y_half);
        }

        let x = ScaleLinear::new(vec![x0, x1], vec![0., plot_width]);
        let y = ScaleLinear::new(vec![y0, y1], vec![height, 10.]);

        let x_labels = (0..=X_TICKS).filter_map(|i| {
            let value = x0 + (x1 - x0) * (i as f64 / X_TICKS as f64);
            x.tick(&value).map(|tick| {
                let align = if i == 0 {
                    TextAlign::Left
                } else if i == X_TICKS {
                    TextAlign::Right
                } else {
                    TextAlign::Center
                };
                let text =
                    if self.x_labels_hex { format!("0x{:X}", value.max(0.0) as u64) } else { format_tick(value) };
                AxisText::new(text, tick, cx.theme().muted_foreground).align(align)
            })
        });
        PlotAxis::new().x(height).x_label(x_labels).stroke(cx.theme().border).paint(&chart_bounds, window, cx);

        let y_tick_values: Vec<f64> = (0..=Y_TICKS).map(|i| y0 + (y1 - y0) * (i as f64 / Y_TICKS as f64)).collect();
        Grid::new()
            .y(y_tick_values.iter().filter_map(|v| y.tick(v)).map(px).collect::<Vec<_>>())
            .stroke(cx.theme().border)
            .dash_array(&[px(4.), px(2.)])
            .paint(&chart_bounds, window);
        let y_labels: Vec<PlotText> = y_tick_values
            .iter()
            .filter_map(|v| {
                y.tick(v).map(|tick| {
                    PlotText::new(
                        format_tick(*v),
                        point(px(-Y_LABEL_MARGIN + 4.0), px(tick)),
                        cx.theme().muted_foreground,
                    )
                    .align(TextAlign::Right)
                })
            })
            .collect();
        PlotLabel::new(y_labels).paint(&chart_bounds, window, cx);

        let color = cx.theme().chart_2;
        match &self.series {
            ChartSeries::Line(points) => {
                Line::new()
                    .data(points.iter().copied())
                    .x(move |p: &(f64, f64)| x.tick(&p.0))
                    .y(move |p: &(f64, f64)| y.tick(&p.1))
                    .stroke(color)
                    .stroke_width(px(1.5))
                    .paint(&chart_bounds, window);
            }
            ChartSeries::Bar(points) => {
                let band = (plot_width / points.len().max(1) as f32 * 0.8).max(1.0);
                let baseline = y.tick(&0.0f64.clamp(y0, y1)).unwrap_or(height);
                Bar::new()
                    .data(points.iter().copied())
                    .band_width(band)
                    .x(move |p: &(f64, f64)| x.tick(&p.0).map(|t| t - band / 2.0))
                    .y0(move |_: &(f64, f64)| baseline)
                    .y1(move |p: &(f64, f64)| y.tick(&p.1))
                    .fill(move |_: &(f64, f64)| color)
                    .paint(&chart_bounds, window, cx);
            }
            ChartSeries::Scatter(points) => {
                // No scatter shape in gpui-component 0.5.1; paint each
                // point as a small filled quad directly.
                const POINT: f32 = 4.0;
                let origin = chart_bounds.origin;
                for p in points {
                    let (Some(tx), Some(ty)) = (x.tick(&p.0), y.tick(&p.1)) else { continue };
                    let quad =
                        bounds(origin + point(px(tx - POINT / 2.0), px(ty - POINT / 2.0)), size(px(POINT), px(POINT)));
                    window.paint_quad(fill(quad, color));
                }
            }
        }
    }
}

/// Paint the coordinates world rect: a 2:1 equirectangular rectangle
/// fitted into `bounds`, equator + prime meridian grid lines, and a
/// dot at the resolved lat/lng (port of egui `coordinates.rs`).
fn paint_world(
    bounds_in: Bounds<Pixels>,
    lat: f64,
    lng: f64,
    bg: gpui::Hsla,
    grid: gpui::Hsla,
    dot: gpui::Hsla,
    window: &mut Window,
) {
    let avail_w = bounds_in.size.width.as_f32();
    let avail_h = bounds_in.size.height.as_f32();
    if avail_w <= 0.0 || avail_h <= 0.0 {
        return;
    }
    let w = avail_w.min(avail_h * 2.0);
    let h = w / 2.0;
    let rect = bounds(bounds_in.origin, size(px(w), px(h)));
    window.paint_quad(fill(rect, bg));
    let to_x = |lng: f64| rect.origin.x + px(((lng + 180.0) / 360.0) as f32 * w);
    let to_y = |lat: f64| rect.origin.y + px(((90.0 - lat) / 180.0) as f32 * h);
    // Equator + prime meridian.
    let equator = bounds(point(rect.origin.x, to_y(0.0)), size(px(w), px(1.0)));
    window.paint_quad(fill(equator, grid));
    let prime = bounds(point(to_x(0.0), rect.origin.y), size(px(1.0), px(h)));
    window.paint_quad(fill(prime, grid));
    const DOT: f32 = 10.0;
    let center = point(to_x(lng), to_y(lat));
    let dot_bounds = bounds(center - point(px(DOT / 2.0), px(DOT / 2.0)), size(px(DOT), px(DOT)));
    window.paint_quad(fill(dot_bounds, dot).corner_radii(gpui::Corners::all(px(DOT / 2.0))));
}

/// `[N bytes]` summary for `BytesVal`, else the shared template value
/// formatter (egui `format_value_for_table` parity).
fn format_table_value(node: &Node, fmts: &TemplateValueFormats) -> String {
    if let Some(Value::BytesVal(b)) = node.value.as_ref() {
        return hxy_i18n::t_args("visualizer-table-bytes-value", &[("n", &b.len().to_string())]);
    }
    format_value(node, fmts, false).unwrap_or_default()
}

struct VisTableDelegate {
    columns: [Column; 5],
    panel: gpui::WeakEntity<VisualizerPanel>,
}

impl VisTableDelegate {
    fn new(panel: gpui::WeakEntity<VisualizerPanel>) -> Self {
        Self {
            columns: [
                Column::new("name", hxy_i18n::t("visualizer-table-col-name")).width(px(200.0)),
                Column::new("type", hxy_i18n::t("visualizer-table-col-type")).width(px(120.0)),
                Column::new("offset", hxy_i18n::t("visualizer-table-col-offset")).width(px(90.0)),
                Column::new("length", hxy_i18n::t("visualizer-table-col-length")).width(px(80.0)),
                Column::new("value", hxy_i18n::t("visualizer-table-col-value")).width(px(220.0)),
            ],
            panel,
        }
    }

    fn row(&self, row_ix: usize, cx: &App) -> Option<VisTableRow> {
        Some(self.panel.upgrade()?.read(cx).table_rows.get(row_ix)?.clone())
    }
}

impl TableDelegate for VisTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, cx: &App) -> usize {
        self.panel.upgrade().map(|p| p.read(cx).table_rows.len()).unwrap_or(0)
    }

    fn column(&self, col_ix: usize, _cx: &App) -> &Column {
        &self.columns[col_ix]
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(row) = self.row(row_ix, cx) else {
            return div().into_any_element();
        };
        let mono = |text: SharedString| {
            div().font_family(cx.theme().mono_font_family.clone()).truncate().child(text).into_any_element()
        };
        match col_ix {
            0 => div().truncate().child(row.name).into_any_element(),
            1 => div().truncate().text_color(cx.theme().muted_foreground).child(row.type_label).into_any_element(),
            2 => mono(row.offset),
            3 => mono(row.length),
            4 => div().truncate().child(row.value).into_any_element(),
            _ => div().into_any_element(),
        }
    }
}

impl Panel for VisualizerPanel {
    fn panel_name(&self) -> &'static str {
        VISUALIZER_PANEL_NAME
    }

    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Image-square prefix mirrors egui's visualizer header glyph;
        // close lives on the dock tab itself (egui's in-header X). Shows
        // only in single-panel title-bar mode -- the multi-tab TabBar
        // renders `tab_name` text, which has no icon slot (0.5.1).
        h_flex().gap_1().items_center().child(Icon::new(HxyIcon::ImageSquare).small()).child(SharedString::from(
            hxy_i18n::t_args("tab-visualizer", &[("name", &tab_label(self.owning_path.as_deref()))]),
        ))
    }

    fn tab_name(&self, _cx: &App) -> Option<SharedString> {
        Some(SharedString::from(hxy_i18n::t_args(
            "tab-visualizer",
            &[("name", &tab_label(self.owning_path.as_deref()))],
        )))
    }

    /// Persist the owning path only. Targets and the active sub-tab
    /// derive from the file's template instances, which the owning
    /// `FilePanel` re-fires on restore; instance ids are not stable
    /// across sessions, so the active key is recomputed, not
    /// round-tripped (mirrors egui persisting only `visualizer_open`).
    fn dump(&self, _cx: &App) -> PanelState {
        let mut state = PanelState::new(self);
        state.info = PanelInfo::panel(serde_json::json!({
            "path": self.owning_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
        }));
        state
    }
}

impl Focusable for VisualizerPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for VisualizerPanel {}

impl Render for VisualizerPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = v_flex().size_full().bg(cx.theme().background);
        if self.owning_file.is_none() {
            return root
                .child(div().p_2().text_color(cx.theme().muted_foreground).child(hxy_i18n::t("visualizer-no-file")));
        }
        if self.targets.is_empty() {
            return root.child(
                div().p_2().text_color(cx.theme().muted_foreground).child(hxy_i18n::t("visualizer-no-targets")),
            );
        }
        let body = self.render_body(cx);
        root.child(self.render_tab_strip(cx))
            .child(div().border_t_1().border_color(cx.theme().border))
            .child(div().flex_1().min_h_0().child(body))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use gpui::TestAppContext;
    use hxy_core::ByteOffset;
    use hxy_core::ByteRange;
    use hxy_core::HexSource;
    use hxy_core::MemorySource;
    use hxy_plugin_host::ParsedTemplate;
    use hxy_plugin_host::template::Arg;
    use hxy_plugin_host::template::NodeType;
    use hxy_plugin_host::template::ResultTree;
    use hxy_plugin_host::template::ScalarKind;
    use hxy_plugin_host::template::Span;
    use hxy_templates::state::TemplateInstance;
    use hxy_templates::state::TemplateInstanceId;
    use hxy_templates::state::TemplateNodeIdx;
    use hxy_templates::state::new_state_from;
    use hxy_vfs::HandlerError;

    use super::*;

    fn setup(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
    }

    struct InertParsed;

    impl ParsedTemplate for InertParsed {
        fn execute(&self, _args: &[Arg]) -> Result<ResultTree, HandlerError> {
            Err(HandlerError::Unsupported("test template never re-executes".into()))
        }

        fn expand_array(&self, _array_id: u64, _start: u64, _end: u64) -> Result<Vec<Node>, HandlerError> {
            Ok(Vec::new())
        }
    }

    fn scalar_node(name: &str, span: (u64, u64), visualize: Option<&str>) -> Node {
        let mut attributes = Vec::new();
        if let Some(spec) = visualize {
            attributes.push((hxy_plugin_host::VISUALIZE_ATTR.to_owned(), spec.to_owned()));
        }
        Node {
            name: name.to_owned(),
            type_name: NodeType::Scalar(ScalarKind::U8K),
            span: Span { offset: span.0, length: span.1 },
            value: None,
            parent: None,
            array: None,
            display: None,
            attributes,
        }
    }

    /// Install `tree` as a completed instance on `file` and return its
    /// id (mirrors the template_view test fixture installer).
    fn install_tree(
        file: &Entity<FilePanel>,
        cx: &mut gpui::VisualTestContext,
        tree: ResultTree,
    ) -> TemplateInstanceId {
        let id = file.update(cx, |file, cx| {
            let state = new_state_from(std::sync::Arc::new(InertParsed), tree, HashMap::new());
            let id = file.fresh_template_instance_id();
            file.upsert_template_instance(TemplateInstance {
                id,
                source_path: PathBuf::from("/tmp/fixture.bt"),
                display_name: "fixture.bt".to_owned(),
                range: ByteRange::new(ByteOffset::new(0), ByteOffset::new(16)).unwrap(),
                source_fingerprint: None,
                state,
            });
            file.active_template = Some(id);
            cx.notify();
            id
        });
        cx.run_until_parked();
        id
    }

    fn tree(nodes: Vec<Node>) -> ResultTree {
        ResultTree { nodes, diagnostics: Vec::new(), byte_palette: None }
    }

    fn build(cx: &mut TestAppContext) -> (Entity<VisualizerPanel>, Entity<FilePanel>, &mut gpui::VisualTestContext) {
        let window = cx.add_window(|window, cx| {
            let source: std::sync::Arc<dyn HexSource> =
                std::sync::Arc::new(MemorySource::new((0u8..16).collect::<Vec<_>>()));
            let file = cx.new(|cx| FilePanel::new(source, None, window, cx));
            let panel = cx.new(|cx| VisualizerPanel::new(file, None, window, cx));
            gpui_component::Root::new(panel, window, cx)
        });
        let root = window.root(cx).unwrap();
        let panel = root.read_with(cx, |root, _| root.view().clone().downcast::<VisualizerPanel>().unwrap());
        let vcx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        vcx.run_until_parked();
        let file = panel.read_with(vcx, |p, _| p.owning_file_for_test().expect("bound at construction"));
        (panel, file, vcx)
    }

    /// The table kind's offset/length cells format with the user's
    /// numeric format from the settings global, not the M4a-era
    /// defaults.
    #[gpui::test]
    fn table_rows_use_settings_numeric_format(cx: &mut TestAppContext) {
        setup(cx);
        cx.update(|cx| {
            let settings = crate::settings::AppSettings {
                numeric_format: hxy_core::format::NumericFormat::Always(hxy_core::format::NumericBase::Decimal),
                ..crate::settings::AppSettings::default()
            };
            crate::settings::init(cx, crate::settings::SettingsBoot { settings, sink: None, failure: None });
        });
        let (panel, file, cx) = build(cx);
        let table = scalar_node("tbl", (0, 12), Some("table"));
        let mut child = scalar_node("field", (10, 4), None);
        child.parent = Some(0);
        install_tree(&file, cx, tree(vec![table, child]));

        let rows = panel.read_with(cx, |p, _| p.table_row_spans_for_test());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "10", "decimal per the settings global, not the 0xA default");
        assert_eq!(rows[0].1, "4");

        // Live-apply: a later format change rebuilds the rows.
        cx.update(|_, cx| {
            crate::settings::update_settings(cx, |s| {
                s.numeric_format = hxy_core::format::NumericFormat::Always(hxy_core::format::NumericBase::Hex);
            });
        });
        cx.run_until_parked();
        let rows = panel.read_with(cx, |p, _| p.table_row_spans_for_test());
        assert_eq!(rows[0].0, "0xA", "format change live-applied to the table rows");
    }

    /// Targets derive from the bound file's template instances: nodes
    /// carrying a visualize attribute become sub-tabs, the first one
    /// becomes active, and plain nodes are skipped.
    #[gpui::test]
    fn targets_derive_from_visualize_attributes(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, file, cx) = build(cx);
        assert_eq!(panel.read_with(cx, |p, _| p.target_count()), 0, "no template run yet");

        let id = install_tree(
            &file,
            cx,
            tree(vec![
                scalar_node("plain", (0, 4), None),
                scalar_node("pixels", (4, 4), Some("digram")),
                scalar_node("wave", (8, 8), Some("sound")),
            ]),
        );

        assert_eq!(panel.read_with(cx, |p, _| p.target_count()), 2);
        assert_eq!(
            panel.read_with(cx, |p, _| p.active_key()),
            Some(VisualizerKey { instance: id, node: TemplateNodeIdx(1) }),
            "the first visualize-bearing node becomes the active sub-tab"
        );
    }

    /// A re-run that renumbers or drops the visualize-bearing node
    /// gc's the stale cache entry and renormalizes the active sub-tab
    /// (egui `VisualizerPanel::gc` parity).
    #[gpui::test]
    fn rerun_gcs_stale_caches_and_renormalizes_active(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, file, cx) = build(cx);
        let id = install_tree(
            &file,
            cx,
            tree(vec![scalar_node("plain", (0, 4), None), scalar_node("pixels", (4, 4), Some("digram"))]),
        );
        let old_key = VisualizerKey { instance: id, node: TemplateNodeIdx(1) };
        assert_eq!(panel.read_with(cx, |p, _| p.active_key()), Some(old_key));
        panel.update(cx, |p, _| p.insert_cache_for_test(old_key));

        // Re-run under the SAME instance id, with the visualizer now
        // on node 0 (renumbered tree).
        install_tree(&file, cx, tree(vec![scalar_node("pixels", (0, 8), Some("digram"))]));
        // upsert under a fresh id replaced nothing -- install_tree
        // allocates a new id, so the old instance is still present;
        // remove it to model the re-run's replace.
        file.update(cx, |file, cx| {
            file.templates.retain(|t| t.id != id);
            cx.notify();
        });
        cx.run_until_parked();

        let new_key = panel.read_with(cx, |p, _| p.active_key()).expect("a target survives");
        assert_ne!(new_key, old_key, "active renormalized onto the new tree's target");
        // Rendering the new active target may have (re)filled ITS
        // cache slot; the invariant is only that the stale key's
        // entry is gone.
        assert!(
            !panel.read_with(cx, |p, _| p.cached_keys()).contains(&old_key),
            "the stale key's cache entry was dropped"
        );
    }

    /// `ensure_texture` only re-decodes when the fingerprint moves --
    /// the cache-invalidation contract every texture kind relies on
    /// across byte edits and template re-runs.
    #[test]
    fn ensure_texture_redecodes_only_on_fingerprint_change() {
        let mut slot: Option<TextureCache> = None;
        let mut decodes = 0;
        let decode = |n: &mut i32| {
            *n += 1;
            Ok((1u32, 1u32, vec![1u8, 2, 3, 4]))
        };
        ensure_texture(&mut slot, [0xAA; 32], || decode(&mut decodes));
        ensure_texture(&mut slot, [0xAA; 32], || decode(&mut decodes));
        assert_eq!(decodes, 1, "same fingerprint reuses the cached texture");
        ensure_texture(&mut slot, [0xBB; 32], || decode(&mut decodes));
        assert_eq!(decodes, 2, "a moved fingerprint re-decodes");
        let slot = slot.expect("filled");
        assert_eq!(slot.size, (1, 1));
        assert!(slot.texture.is_some());
        assert!(slot.error.is_none());
    }

    /// A decode error is cached under its fingerprint too (no
    /// re-decode storm on every render) and clears once the bytes
    /// change to something decodable.
    #[test]
    fn ensure_texture_caches_errors_per_fingerprint() {
        let mut slot: Option<TextureCache> = None;
        ensure_texture(&mut slot, [1; 32], || Err("nope".to_owned()));
        assert_eq!(slot.as_ref().and_then(|s| s.error.as_deref()), Some("nope"));
        ensure_texture(&mut slot, [2; 32], || Ok((1, 1, vec![0, 0, 0, 0xFF])));
        let slot = slot.expect("filled");
        assert!(slot.error.is_none());
        assert!(slot.texture.is_some());
    }

    /// `RenderImage` stores BGRA byte order; the builder must swap the
    /// red/blue channels of the decoders' RGBA output. 2x1 fixture:
    /// RGBA (1,2,3,4),(5,6,7,8) must land as BGRA (3,2,1,4),(7,6,5,8).
    #[test]
    fn render_image_swaps_rgba_to_bgra() {
        let image = render_image_from_rgba(2, 1, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(image.as_bytes(0), Some(&[3u8, 2, 1, 4, 7, 6, 5, 8][..]));
        let size = image.size(0);
        assert_eq!((u32::from(size.width), u32::from(size.height)), (2, 1));
    }

    #[gpui::test]
    fn dump_round_trips_the_owning_path(cx: &mut TestAppContext) {
        setup(cx);
        let path = PathBuf::from("/tmp/hxy-visualizer-fixture.bin");
        let window = cx.add_window(|window, cx| {
            let source: std::sync::Arc<dyn HexSource> = std::sync::Arc::new(MemorySource::new(vec![0u8; 4]));
            let file =
                cx.new(|cx| FilePanel::new(source, Some(PathBuf::from("/tmp/hxy-visualizer-fixture.bin")), window, cx));
            let panel = cx.new(|cx| {
                VisualizerPanel::new(file, Some(PathBuf::from("/tmp/hxy-visualizer-fixture.bin")), window, cx)
            });
            gpui_component::Root::new(panel, window, cx)
        });
        let root = window.root(cx).unwrap();
        let panel = root.read_with(cx, |root, _| root.view().clone().downcast::<VisualizerPanel>().unwrap());
        let dumped = panel.read_with(cx, |p, cx| p.dump(cx));
        assert_eq!(dumped.panel_name, VISUALIZER_PANEL_NAME);
        assert_eq!(path_from_info(&dumped.info), Some(path));
    }

    #[test]
    fn path_from_info_defaults_to_none_for_older_or_empty_layouts() {
        let info = PanelInfo::panel(serde_json::json!({}));
        assert_eq!(path_from_info(&info), None);
    }

    /// Restore before the owning file reopens: the panel starts
    /// unbound and picks the file up once `OpenFilePanels` publishes,
    /// then derives its targets from the file's instances (mirrors the
    /// entropy panel's restore-rebind test).
    #[gpui::test]
    fn restore_rebinds_when_global_updates_later(cx: &mut TestAppContext) {
        setup(cx);
        let path = PathBuf::from("/tmp/hxy-visualizer-restore.bin");
        let info = PanelInfo::panel(serde_json::json!({ "path": path.to_string_lossy() }));
        let window = cx.add_window(move |window, cx| {
            let panel = cx.new(|cx| VisualizerPanel::restore(&info, window, cx));
            gpui_component::Root::new(panel, window, cx)
        });
        let root = window.root(cx).unwrap();
        let panel = root.read_with(cx, |root, _| root.view().clone().downcast::<VisualizerPanel>().unwrap());
        let cx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        cx.run_until_parked();
        assert!(panel.read_with(cx, |p, _| p.owning_file_for_test().is_none()), "no file open yet");

        let path_for_closure = path.clone();
        cx.update(|window, cx| {
            let source: std::sync::Arc<dyn HexSource> = std::sync::Arc::new(MemorySource::new(vec![0u8; 16]));
            let file = cx.new(|cx| FilePanel::new(source, Some(path_for_closure), window, cx));
            cx.set_global(OpenFilePanels(vec![file]));
        });
        cx.run_until_parked();

        assert!(panel.read_with(cx, |p, _| p.owning_file_for_test().is_some()));
    }
}
