//! Per-template visualizer panel.
//!
//! ImHex's `[[hex::visualize("name", arg1, arg2, ...)]]` attribute
//! turns a field into a renderable artifact: an image, a waveform,
//! a disassembly. Any runtime that emits the canonical `hxy_visualize`
//! / `hxy_inline_visualize` attribute (see
//! [`hxy_plugin_host::VISUALIZE_ATTR`]) drives the same dispatch
//! here, so a 010 plugin or a future WASM template gets the
//! visualizers for free.
//!
//! The attribute value is a packed string -- `name<US>arg1<US>arg2`
//! where `<US>` is ASCII 0x1F (see
//! [`hxy_plugin_host::VISUALIZE_ARG_SEP`]). [`VisualizerSpec::parse`]
//! splits it back apart for the renderer.
//!
//! Per-node renderer state (texture handles, decoded images, audio
//! buffers) lives on [`VisualizerCache`], keyed by file + node so a
//! large image isn't re-decoded every frame and so closing one
//! visualizer doesn't drop the cache for another.
//!
//! The [`Tab::Visualizer`](crate::tabs::Tab::Visualizer) dock tab is
//! opened automatically the first time a parsed template on a file
//! contains a visualizer attribute. Closing the tab is sticky for
//! the file's lifetime so re-runs on the same file don't reopen it
//! against the user's wishes -- see [`VisualizerPanel::dismissed`].

#![cfg(not(target_arch = "wasm32"))]

mod bitmap;
mod coordinates;
mod digram;
mod disassembler;
mod distribution;
mod hex_viewer;
mod image;
mod plot;
mod sound;
mod table;
mod text;
mod three_d;
mod timestamp;

use std::collections::HashMap;
use std::sync::Arc;

use hxy_core::HexSource;
use hxy_plugin_host::INLINE_VISUALIZE_ATTR;
use hxy_plugin_host::VISUALIZE_ARG_SEP;
use hxy_plugin_host::VISUALIZE_ATTR;
use hxy_plugin_host::template::Node;
use hxy_plugin_host::template::ResultTree;

use crate::files::OpenFile;
use crate::files::TemplateInstanceId;
use crate::files::TemplateNodeIdx;

/// Decoded visualizer attribute: the named kind plus the user-
/// supplied args. `Unknown` covers unregistered names so the panel
/// can render a clear "not yet supported" placeholder rather than
/// silently dropping the field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualizerSpec {
    pub kind: VisualizerKind,
    /// Raw post-name args, in source order. Visualizer-specific
    /// parsing (number coercion, format-string lookup) happens
    /// inside each renderer; the spec just hands them through.
    pub args: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VisualizerKind {
    Image,
    Bitmap,
    HexViewer,
    Text,
    ChunkEntropy,
    Digram,
    LayeredDistribution,
    LinePlot,
    BarChart,
    ScatterPlot,
    Sound,
    Disassembler,
    Coordinates,
    Timestamp,
    Table,
    ThreeD,
    Unknown(String),
}

impl VisualizerKind {
    /// Short label for the sub-tab strip / row icon tooltip.
    pub fn label(&self) -> &str {
        match self {
            Self::Image => "image",
            Self::Bitmap => "bitmap",
            Self::HexViewer => "hex_viewer",
            Self::Text => "text",
            Self::ChunkEntropy => "chunk_entropy",
            Self::Digram => "digram",
            Self::LayeredDistribution => "layered_distribution",
            Self::LinePlot => "line_plot",
            Self::BarChart => "bar_chart",
            Self::ScatterPlot => "scatter_plot",
            Self::Sound => "sound",
            Self::Disassembler => "disassembler",
            Self::Coordinates => "coordinates",
            Self::Timestamp => "timestamp",
            Self::Table => "table",
            Self::ThreeD => "3d",
            Self::Unknown(name) => name.as_str(),
        }
    }
}

impl VisualizerSpec {
    /// Parse a packed `name<US>arg1<US>...` attribute value back into
    /// a kind + args list. An empty string returns `None` (caller
    /// treats the attribute as absent); a non-empty value with an
    /// empty name produces `Unknown("")` so the panel can surface
    /// the malformed attribute instead of silently ignoring it.
    pub fn parse(raw: &str) -> Option<Self> {
        if raw.is_empty() {
            return None;
        }
        let mut parts = raw.split(VISUALIZE_ARG_SEP);
        let name = parts.next()?.to_owned();
        let args: Vec<String> = parts.map(|s| s.to_owned()).collect();
        let kind = match name.as_str() {
            "image" => VisualizerKind::Image,
            "bitmap" => VisualizerKind::Bitmap,
            "hex_viewer" => VisualizerKind::HexViewer,
            "text" => VisualizerKind::Text,
            "chunk_entropy" => VisualizerKind::ChunkEntropy,
            "digram" => VisualizerKind::Digram,
            "layered_distribution" => VisualizerKind::LayeredDistribution,
            "line_plot" => VisualizerKind::LinePlot,
            "bar_chart" => VisualizerKind::BarChart,
            "scatter_plot" => VisualizerKind::ScatterPlot,
            "sound" => VisualizerKind::Sound,
            "disassembler" => VisualizerKind::Disassembler,
            "coordinates" => VisualizerKind::Coordinates,
            "timestamp" => VisualizerKind::Timestamp,
            "table" => VisualizerKind::Table,
            "3d" => VisualizerKind::ThreeD,
            other => VisualizerKind::Unknown(other.to_owned()),
        };
        Some(Self { kind, args })
    }
}

/// Read the visualizer / inline_visualizer attribute off `node` (if
/// present) and parse it. Returns the spec and whether it was the
/// inline variant. `None` when the node has neither attribute.
pub fn read_node_visualizer(node: &Node) -> Option<(VisualizerSpec, Inline)> {
    if let Some(spec) = lookup_visualizer(node, VISUALIZE_ATTR) {
        return Some((spec, Inline::No));
    }
    if let Some(spec) = lookup_visualizer(node, INLINE_VISUALIZE_ATTR) {
        return Some((spec, Inline::Yes));
    }
    None
}

fn lookup_visualizer(node: &Node, key: &str) -> Option<VisualizerSpec> {
    let raw = node.attributes.iter().find_map(|(k, v)| (k == key).then_some(v.as_str()))?;
    VisualizerSpec::parse(raw)
}

/// Whether a visualizer was declared as the inline variant. Inline
/// visualizers also render in the template-panel row (small thumbnail);
/// the popout tab still applies for both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inline {
    No,
    Yes,
}

/// Identity of one visualizer instance: the template instance + tree
/// node it lives on. Stable for the lifetime of the template run; a
/// re-run that drops the node also drops the cache entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VisualizerKey {
    pub instance: TemplateInstanceId,
    pub node: TemplateNodeIdx,
}

/// Per-node renderer state (texture handles, decoded buffers).
/// Lives on [`VisualizerPanel::cache`]; entries are dropped when the
/// owning template instance is replaced (a fresh re-run starts with
/// an empty cache, but the panel itself stays open / dismissed).
#[derive(Default)]
pub struct VisualizerCache {
    /// Decoded image texture, keyed by content fingerprint so a
    /// re-run that produced the same bytes reuses the GPU texture.
    pub image: Option<image::ImageCache>,
    /// Raw-bitmap texture cache, same fingerprinting story.
    pub bitmap: Option<bitmap::BitmapCache>,
    /// Digram heatmap texture.
    pub digram: Option<digram::DigramCache>,
    /// Layered distribution heatmap texture.
    pub distribution: Option<distribution::DistributionCache>,
    /// Audio waveform downsample, computed once per byte fingerprint.
    pub sound: Option<sound::SoundCache>,
    /// Disassembly listing, decoded once and reused across frames
    /// (the listing can be tens of kB and parsing every frame is
    /// pointless).
    pub disassembler: Option<disassembler::DisassemblerCache>,
}

/// Per-file panel state. Owns the cache map plus the dismissed flag
/// (so closing the panel via the X button stays closed across
/// repaints) and the active visualizer key (the sub-tab the user
/// last selected).
pub struct VisualizerPanel {
    pub cache: HashMap<VisualizerKey, VisualizerCache>,
    /// True after the user explicitly closed the dock tab. Suppresses
    /// the auto-open path that would otherwise re-show the tab on
    /// the next template re-run.
    pub dismissed: bool,
    /// Key of the visualizer currently rendering in the body. `None`
    /// = pick the first available target.
    pub active: Option<VisualizerKey>,
    /// Set true by the in-row visualizer icon click handler. The
    /// post-dock-pass drain calls `show_visualizer_for(file_id)` and
    /// clears the flag. Held on the panel rather than a free-form
    /// app sink because the click handler only sees `&mut OpenFile`,
    /// not the dock state.
    pub pending_show: bool,
}

impl Default for VisualizerPanel {
    fn default() -> Self {
        Self { cache: HashMap::new(), dismissed: false, active: None, pending_show: false }
    }
}

impl VisualizerPanel {
    /// Drop cache entries that no longer have a backing target in
    /// the file. Called after a template re-run swaps trees so we
    /// don't leak GPU textures keyed by stale node ids.
    pub fn gc(&mut self, live_keys: &std::collections::HashSet<VisualizerKey>) {
        self.cache.retain(|k, _| live_keys.contains(k));
        if let Some(active) = self.active
            && !live_keys.contains(&active)
        {
            self.active = None;
        }
    }
}

/// A visualizer-bearing field discovered in a template result tree.
/// One per node that carries a visualizer attribute; a node with both
/// `hxy_visualize` and `hxy_inline_visualize` only emits the popout
/// here (the inline marker is handled by the template panel).
pub struct VisualizerTarget {
    pub key: VisualizerKey,
    pub spec: VisualizerSpec,
    /// Display name for the sub-tab strip: the field's localized
    /// name (or `[idx]` for unnamed array elements). Built once when
    /// the target is collected so the strip render doesn't re-walk
    /// the tree.
    pub label: String,
    pub byte_offset: u64,
    pub byte_length: u64,
}

/// Walk a file's completed templates and return every visualizer
/// target across all of them. Used by the panel to populate its
/// sub-tab strip and by the auto-open path to decide whether to
/// surface the tab at all.
pub fn collect_targets(file: &OpenFile) -> Vec<VisualizerTarget> {
    let mut out = Vec::new();
    for instance in &file.templates {
        collect_from_tree(instance.id, &instance.state.tree, &mut out);
    }
    out
}

fn collect_from_tree(instance: TemplateInstanceId, tree: &ResultTree, out: &mut Vec<VisualizerTarget>) {
    for (idx, node) in tree.nodes.iter().enumerate() {
        let Some((spec, _)) = read_node_visualizer(node) else { continue };
        let label = node.name.clone();
        out.push(VisualizerTarget {
            key: VisualizerKey { instance, node: TemplateNodeIdx(idx as u32) },
            spec,
            label,
            byte_offset: node.span.offset,
            byte_length: node.span.length,
        });
    }
}

/// Render the visualizer dock tab body for `file_id`. Returns events
/// the host needs to act on after the dock pass releases its borrow.
pub fn show(
    ui: &mut egui::Ui,
    file: Option<&OpenFile>,
    panel: &mut VisualizerPanel,
    numeric_format: crate::settings::NumericFormat,
    template_value_format: crate::settings::NumericFormat,
) -> Vec<VisualizerEvent> {
    let mut events = Vec::new();
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("{} Visualizer", egui_phosphor::regular::IMAGE_SQUARE)).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add(egui::Button::new(egui_phosphor::regular::X).frame(false))
                .on_hover_text(hxy_i18n::t("visualizer-close"))
                .clicked()
            {
                events.push(VisualizerEvent::Dismiss);
            }
        });
    });
    ui.separator();

    let Some(file) = file else {
        ui.weak(hxy_i18n::t("visualizer-no-file"));
        return events;
    };
    let targets = collect_targets(file);
    if targets.is_empty() {
        ui.weak(hxy_i18n::t("visualizer-no-targets"));
        return events;
    }

    if panel.active.is_none() || !targets.iter().any(|t| Some(t.key) == panel.active) {
        panel.active = Some(targets[0].key);
    }

    egui::ScrollArea::horizontal().id_salt(("hxy-visualizer-strip", file.id.get())).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for target in &targets {
                let active = Some(target.key) == panel.active;
                let label = format!("{} ({})", target.label, target.spec.kind.label());
                if ui.add(egui::Button::selectable(active, label)).clicked() {
                    panel.active = Some(target.key);
                }
            }
        });
    });
    ui.separator();

    let active_key = panel.active.expect("set above");
    let Some(target) = targets.iter().find(|t| t.key == active_key) else {
        ui.weak(hxy_i18n::t("visualizer-no-targets"));
        return events;
    };

    let Some(instance) = file.templates.iter().find(|t| t.id == active_key.instance) else {
        ui.weak(hxy_i18n::t("visualizer-no-targets"));
        return events;
    };
    let node = match instance.state.tree.nodes.get(active_key.node.0 as usize) {
        Some(n) => n,
        None => {
            ui.weak(hxy_i18n::t("visualizer-no-targets"));
            return events;
        }
    };

    let bytes = match read_field_bytes(file, target.byte_offset, target.byte_length) {
        Ok(b) => b,
        Err(e) => {
            ui.colored_label(ui.visuals().error_fg_color, e);
            return events;
        }
    };

    let cache = panel.cache.entry(active_key).or_default();
    let ctx = VisualizerContext {
        bytes: &bytes,
        spec: &target.spec,
        node,
        tree: &instance.state.tree,
        source: file.editor.source().clone(),
        ui_id: ui.id().with(active_key),
        numeric_format,
        template_value_format,
    };
    render_kind(ui, &ctx, cache);
    events
}

/// Read `length` bytes starting at `offset` from the file's source.
/// Returns a localized error string on out-of-range / read failures
/// so the panel can render it without each visualizer re-doing the
/// boundary checks.
fn read_field_bytes(file: &OpenFile, offset: u64, length: u64) -> Result<Vec<u8>, String> {
    if length == 0 {
        return Ok(Vec::new());
    }
    let end = offset.saturating_add(length);
    let range = hxy_core::ByteRange::new(hxy_core::ByteOffset::new(offset), hxy_core::ByteOffset::new(end))
        .map_err(|e| hxy_i18n::t_args("visualizer-read-error", &[("error", &format!("{e}"))]))?;
    file.editor
        .source()
        .read(range)
        .map_err(|e| hxy_i18n::t_args("visualizer-read-error", &[("error", &format!("{e}"))]))
}

/// Context the host hands every visualizer renderer. `ui_id` is a
/// unique salt scoped to the active visualizer so widget ids inside
/// each renderer don't collide with neighbouring ones in the same
/// dock tab.
pub struct VisualizerContext<'a> {
    pub bytes: &'a [u8],
    pub spec: &'a VisualizerSpec,
    pub node: &'a Node,
    pub tree: &'a ResultTree,
    /// Underlying byte source -- some visualizers (notably `table`)
    /// need to decode child fields whose bytes don't fit inside
    /// `bytes` (which is just the visualized field's slice).
    pub source: Arc<dyn HexSource>,
    pub ui_id: egui::Id,
    /// User-configured base / threshold for span values
    /// (offsets, lengths, end positions). Same setting the
    /// template panel and breadcrumb tooltip use.
    pub numeric_format: crate::settings::NumericFormat,
    /// User-configured base / threshold for template scalar
    /// field values. Used by the table visualizer's Value
    /// column so it stays consistent with the template panel.
    pub template_value_format: crate::settings::NumericFormat,
}

fn render_kind(ui: &mut egui::Ui, ctx: &VisualizerContext, cache: &mut VisualizerCache) {
    match &ctx.spec.kind {
        VisualizerKind::Image => image::show(ui, ctx, cache),
        VisualizerKind::Bitmap => bitmap::show(ui, ctx, cache),
        VisualizerKind::HexViewer => hex_viewer::show(ui, ctx),
        VisualizerKind::Text => text::show(ui, ctx),
        VisualizerKind::ChunkEntropy => plot::show_chunk_entropy(ui, ctx),
        VisualizerKind::Digram => digram::show(ui, ctx, cache),
        VisualizerKind::LayeredDistribution => distribution::show(ui, ctx, cache),
        VisualizerKind::LinePlot => plot::show_line(ui, ctx),
        VisualizerKind::BarChart => plot::show_bar(ui, ctx),
        VisualizerKind::ScatterPlot => plot::show_scatter(ui, ctx),
        VisualizerKind::Sound => sound::show(ui, ctx, cache),
        VisualizerKind::Disassembler => disassembler::show(ui, ctx, cache),
        VisualizerKind::Coordinates => coordinates::show(ui, ctx),
        VisualizerKind::Timestamp => timestamp::show(ui, ctx),
        VisualizerKind::Table => table::show(ui, ctx),
        VisualizerKind::ThreeD => three_d::show(ui, ctx),
        VisualizerKind::Unknown(name) => {
            let msg = hxy_i18n::t_args("visualizer-unknown", &[("name", name)]);
            ui.label(egui::RichText::new(msg).weak());
        }
    }
}

pub enum VisualizerEvent {
    /// User X-clicked the panel header. Caller closes the dock tab
    /// and sets `panel.dismissed = true`.
    Dismiss,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bare_name() {
        let s = VisualizerSpec::parse("image").expect("parses");
        assert_eq!(s.kind, VisualizerKind::Image);
        assert!(s.args.is_empty());
    }

    #[test]
    fn parse_with_args() {
        let raw = format!("bitmap{sep}RGBA8{sep}800{sep}600", sep = VISUALIZE_ARG_SEP);
        let s = VisualizerSpec::parse(&raw).expect("parses");
        assert_eq!(s.kind, VisualizerKind::Bitmap);
        assert_eq!(s.args, vec!["RGBA8", "800", "600"]);
    }

    #[test]
    fn parse_unknown() {
        let s = VisualizerSpec::parse("not_a_real_visualizer").expect("parses");
        assert!(matches!(s.kind, VisualizerKind::Unknown(ref n) if n == "not_a_real_visualizer"));
    }

    #[test]
    fn parse_empty_returns_none() {
        assert!(VisualizerSpec::parse("").is_none());
    }
}
