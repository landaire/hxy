# M4 reference: egui visualizers map (verified 2026-08-20)

Working notes for the M4b port. ~1800 lines across 14 files in `crates/hxy/src/visualizers/`, all egui-facing. The gpui app has none of it. Precedent for charts: `gpui/hxy-gpui/src/panels/entropy.rs`.

KEY FACT: visualizers do NOT sync selection/hover with the hex view. Every renderer is a pure function of a byte slice + `VisualizerSpec`. Only interactivity: egui_plot pan/zoom/hover and ScrollArea scroll/fit. Click-to-jump/selection-sync would be NEW behavior, not parity.

## Dispatcher (mod.rs, 493 lines)

Pure (port verbatim):
- `VisualizerSpec { kind: VisualizerKind, args: Vec<String> }` (L63); `VisualizerKind` (L71-90): Image, Bitmap, HexViewer, Text, ChunkEntropy, Digram, LayeredDistribution, LinePlot, BarChart, ScatterPlot, Sound, Disassembler, Coordinates, Timestamp, Table, ThreeD, Unknown(String); `label()` (L94).
- `VisualizerSpec::parse(raw) -> Option<Self>` (L123) splits on VISUALIZE_ARG_SEP (0x1F). Tests L464-493.
- `read_node_visualizer(node) -> Option<(VisualizerSpec, Inline)>` (L156), `lookup_visualizer` (L166), `Inline` (L174).
- `VisualizerKey { instance: TemplateInstanceId, node: TemplateNodeIdx }` (L184); `VisualizerTarget { key, spec, label, byte_offset, byte_length }` (L255); `collect_targets(file) -> Vec<VisualizerTarget>` (L271) walks `file.templates[].state.tree.nodes`; `read_field_bytes(file, offset, length) -> Result<Vec<u8>, String>` (L392) via ByteRange + HexSource::read.

egui-coupled (rewrite): `VisualizerCache` (L194, TextureHandle sub-caches -> gpui `Arc<RenderImage>`); `VisualizerPanel { cache: HashMap<VisualizerKey, VisualizerCache>, open, active, pending_show }` (L217) + `gc()` (L241); `show(ui, file, panel, numeric_format, template_value_formats) -> Vec<VisualizerEvent>` (L295, sub-tab strip + dispatch; VisualizerEvent::Dismiss L458); `VisualizerContext<'a>` (L409: bytes, spec, node, tree, source, ui_id, numeric_format, template_value_formats, inverse_format); `render_kind` (L433).

## Data model

WIT types from `hxy_plugin_host::template`: Span{offset,length}; Node{name, type_name, span, value, parent: Option<u32> (flat pre-order list, parent = index), array, display, attributes}; Value (U8Val..S64Val, F32/F64Val, BoolVal, BytesVal, StringVal, EnumVal((String,u64))); NodeType (Scalar, ScalarArray, StructType, StructArray, EnumType, EnumArray, Unknown); ScalarKind; DisplayHint {hex,decimal,binary,ascii,timestamp,color}; ResultTree{nodes, diagnostics, byte_palette: Option<Vec<u32>>}. Helpers: `node_display_type` (bitfield-aware), `node_type_label`, `scalar_kind_name`. Most visualizers consume only ctx.bytes; exceptions: table.rs (tree children), disassembler.rs (span.offset base), hex_viewer.rs (span.offset base).

## Per-file inventory

Texture group (decode -> RGBA -> texture; gpui: `Arc<gpui::RenderImage>` via `RenderImage::new(SmallVec<[Frame;_]>)` + `img()` element; GOTCHA: RenderImage expects BGRA byte order -- swap channels or build BGRA):
- image.rs (84): `ImageCache { fingerprint: Option<[u8;32]>, texture, size, error }` (L11); `show` (L24) blake3 fingerprint -> `image::load_from_memory` -> to_rgba8; ScrollArea::both + fit-to-width (L74-83). Decode pure.
- bitmap.rs (191): `BitmapCache` (L11); `BitmapFormat` (L19: Rgba8/Rgb8/Bgra8/Bgr8/Gray8/GrayAlpha8/Rgba16Le, parse L33, bytes_per_pixel L46); `decode(ctx) -> Result<(u32,u32,Vec<u8>), String>` (L108) pure channel expansion; `parse_args` (L162); `blake3_short_with_args` (L180). NEAREST filtering. Args: bitmap(format, width, height).
- digram.rs (84): 256x256 byte-pair heatmap; `build_texture` (L43) counts windows(2), log-scale, `viridis(v)` LUT (L74). SIDE=256. Counting + viridis pure.
- distribution.rs (98): layered_distribution position x value heatmap; `build_texture` (L50) chunked columns, per-column histogram, log, `heat(v)` LUT (L89). HEIGHT=256, TARGET_COLS=256.

Plot group (egui_plot -> gpui-component plot primitives; entropy.rs precedent: `#[derive(IntoPlot)]` struct implementing `gpui_component::plot::Plot::paint` L437 with scale::ScaleLinear L456, shape::Line L498, PlotAxis L472, Grid L474, PlotLabel/label::Text L480, AxisText L469; available shapes in 0.5.1: line, area, bar, arc, pie, radial_line, sankey, stack -- NO scatter shape, needs custom point paint):
- plot.rs (172): `Sample` enum (L23, U8..F64 x le/be; parse L39, width L56, read L65); `parse_sample` (L82); `samples(bytes, sample) -> Vec<f64>` (L86) -- all pure. Entry points: `show_line` (L90), `show_bar` (L103), `show_scatter` (L116, pairs consecutive samples, data_aspect 1.0), `show_chunk_entropy` (L134, reuses hxy_panels::entropy::{shannon_entropy, pick_window_size, MAX_ENTROPY}; fixed bounds [0,0]..[len, MAX_ENTROPY] L166).
- sound.rs (125): `SoundCache { fingerprint, samples, channels, sample_rate }` (L15); `SampleFormat` PcmU8/PcmS16Le/PcmS16Be/PcmF32Le (L23); `downsample_for_plot` (L99) pure bucketed averaging to TARGET=4096. No playback.

Text/table group:
- text.rs (66): `show` (L12) decodes utf-8/ascii/latin-1/utf-16le/be, U+FFFD, MAX_BYTES=1MiB; `decode_utf16` (L55). Pure decode; read-only multiline TextEdit.
- hex_viewer.rs (67): `format_dump(base_offset, bytes) -> String` (L44) pure; COLS=16, MAX_BYTES=64KiB; monospace TextEdit.
- table.rs (69): renders node's direct children as Name/Type/Offset/Length/Value grid; child walk via ptr::eq position + parent filter (L14-19); `format_span` (L52, uses format_offset + NumericFormat), `format_value_for_table` (L63, delegates to panels::template::format_value, BytesVal -> "[N bytes]"). egui::Grid.
- disassembler.rs (131): `DisassemblerCache { fingerprint, listing: String, instruction_count, error }` (L17); `Bitness` (L25); `show` (L51, base defaults ctx.node.span.offset, isa default x86-64); `disassemble_x86(bytes, bitness, base, cache)` (L91) via iced-x86 Decoder + IntelFormatter -- pure string. ARM/RISC-V unsupported message.

Readout group:
- coordinates.rs (79): `resolve_coordinates` (L56) args-or-two-f64s, clamp_lat/lng (L74/77) pure; `show` (L10) equirectangular world rect + dot via allocate_painter -- REAL custom paint to rewrite (gpui canvas + paint_quad).
- timestamp.rs (108): `decode(bytes, format) -> Result<jiff::Timestamp, String>` (L42): unix/unix64/unix_ms/unix_us/windows(FILETIME)/mac(HFS+). Fully pure; label render.
- three_d.rs (20): placeholder "not yet supported".

## Port guidance

- Reuse as-is: hxy_panels::entropy, WIT types, hxy_core::{HexSource, ByteRange}.
- Extract pure transforms to shared code (natural home: `hxy-templates` crate created in M4a, or a `visualize` module inside it): VisualizerSpec/Kind/parse, all decode/sample/format/heat-grid fns, Sample/SampleFormat/BitmapFormat/Bitness enums, viridis/heat LUTs. Formatter deps (NumericFormat etc.) move to hxy-core in M4a Task 2.
- Rewrite per-frontend: show() bodies (texture upload, egui_plot, TextEdit, Grid, painter) -> gpui img()/plot/text/table/canvas; VisualizerPanel/Context/tab-strip dispatcher -> gpui Entity modeled on EntropyPanel.
- Effort ranking (low->high): timestamp/three_d/text/hex_viewer -> table -> image/bitmap/digram/distribution (BGRA caveat) -> line/bar/chunk_entropy/sound -> scatter (custom points) -> coordinates (canvas) -> disassembler (listing widget).
- egui docks visualizers as `Tab::Visualizer(FileId)`; gpui: a per-file dockable panel (VisualizerPanel registered name) following the add-a-panel recipe.
- i18n: `tab-visualizer` ($name), `visualizer-close`, `visualizer-no-file`, `visualizer-no-targets`, `visualizer-read-error` ($error), `visualizer-row-tooltip` ($name), `visualizer-unknown` ($name), plus per-kind `visualizer-image-*`, `-bitmap-*`, `-hex-*`, `-text-*`, `-plot-*`, `-entropy-*`, `-digram-*`, `-distribution-*`, `-sound-*`, `-disasm-*`, `-coords-*`, `-timestamp-*`, `-table-*`, `-3d-*`.
- Template->calculator bridge exists (commands/palette/calculator.rs TemplateFieldResolver impl hxy_calculator::PathResolver, FieldRef from node spans) -- part of template palette work, not visualizers.
