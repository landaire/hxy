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
//! opt-in: it stays closed by default and only opens when the user
//! explicitly asks for it (per-row icon click, palette command, or
//! a previous session restoring an "open" state). The user's choice
//! persists across template re-runs and across app restarts -- see
//! [`VisualizerPanel::open`].

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
use hxy_plugin_host::template::Node;
use hxy_plugin_host::template::ResultTree;
pub use hxy_templates::visualize::Inline;
pub use hxy_templates::visualize::VisualizerKey;
pub use hxy_templates::visualize::VisualizerKind;
pub use hxy_templates::visualize::VisualizerSpec;
pub use hxy_templates::visualize::VisualizerTarget;
pub use hxy_templates::visualize::read_node_visualizer;

use crate::files::OpenFile;

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
#[derive(Default)]
pub struct VisualizerPanel {
    pub cache: HashMap<VisualizerKey, VisualizerCache>,
    /// True when the user has explicitly opened the visualizer dock
    /// tab for this file. Default `false` keeps the panel closed,
    /// even when a template emits visualizer attributes; the panel
    /// only pops on (a) explicit user action this session, or (b) a
    /// restored value from a previous session via
    /// `OpenTabState::visualizer_open`.
    pub open: bool,
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

/// Walk a file's completed templates and return every visualizer
/// target across all of them. Thin wrapper over the shared
/// [`hxy_templates::visualize::collect_targets`] so egui call sites
/// keep passing the whole file.
pub fn collect_targets(file: &OpenFile) -> Vec<VisualizerTarget> {
    hxy_templates::visualize::collect_targets(&file.templates)
}

/// Render the visualizer dock tab body for `file_id`. Returns events
/// the host needs to act on after the dock pass releases its borrow.
pub fn show(
    ui: &mut egui::Ui,
    file: Option<&OpenFile>,
    panel: &mut VisualizerPanel,
    numeric_format: crate::settings::NumericFormat,
    template_value_formats: &crate::settings::TemplateValueFormats,
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

    let source = file.editor.source();
    let bytes = match hxy_templates::visualize::read_field_bytes(source, target.byte_offset, target.byte_length) {
        Ok(b) => b,
        Err(e) => {
            ui.colored_label(ui.visuals().error_fg_color, e);
            return events;
        }
    };

    let cache = panel.cache.entry(active_key).or_default();
    let inverse_format = ui.input(|i| i.modifiers.alt);
    let ctx = VisualizerContext {
        bytes: &bytes,
        spec: &target.spec,
        node,
        tree: &instance.state.tree,
        source: file.editor.source().clone(),
        ui_id: ui.id().with(active_key),
        numeric_format,
        template_value_formats,
        inverse_format,
    };
    render_kind(ui, &ctx, cache);
    events
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
    /// Per-integer-type formats for template scalar field
    /// values. Used by the table visualizer's Value column so
    /// it stays consistent with the template panel.
    pub template_value_formats: &'a crate::settings::TemplateValueFormats,
    /// True while the inverse-format modifier (Alt / Option) is
    /// held, mirroring the behaviour the template panel and
    /// hover tooltip use.
    pub inverse_format: bool,
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
    /// and sets `panel.open = false`.
    Dismiss,
}
