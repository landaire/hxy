# M4b: GPUI Visualizers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Port the 16 template-driven visualizers (image, bitmap, hex dump, text, chunk entropy, digram, layered distribution, line/bar/scatter plots, sound waveform, disassembler, coordinates, timestamp, table, 3d placeholder) to hxy-gpui, wired to template results via the OpenVisualizer event.

**Architecture:** Pure decode/transform logic moves from `crates/hxy/src/visualizers/*` into `crates/hxy-templates/src/visualize/` (verbatim, Rgba/plain types); egui refactored onto it unchanged. gpui gets a registered dockable per-file `VisualizerPanel` (add-a-panel recipe) with a sub-tab strip of targets, fingerprinted caches, `Arc<gpui::RenderImage>` textures (BGRA byte order), gpui-component plot primitives per the EntropyPanel precedent, and canvas paint for coordinates.

**Reference maps (read first):** docs/superpowers/plans/2026-08-20-m4-visualizers-map.md (file-by-file inventory + gpui facilities), 2026-08-20-m4-gpui-architecture-map.md (add-a-panel recipe). M4a landed: `hxy-templates` (state/format/breadcrumb), gpui `templates.rs` + `panels/template_view.rs` + FilePanel reducer with `TemplateEvent::OpenVisualizer` currently a debug-log stub.

## Global Constraints

Same as 2026-08-20-m4a-gpui-templates.md Global Constraints (both workspaces green at every task end, jj conventional commits no attribution, ASCII, i18n everything user-visible, typed errors, verbatim extraction discipline, fresh-subagent review per task, serial cargo runs, chmod -R u+w ~/.cargo/build on *-sys PermissionDenied, buckify on root manifest changes).

---

### Task 1: Extract pure visualizer core into hxy-templates::visualize

**Files:** Create `crates/hxy-templates/src/visualize/mod.rs` (+ per-kind modules as the code splits naturally: `bitmap.rs`, `grids.rs`, `plot.rs`, `sound.rs`, `text.rs`, `hex_dump.rs`, `disasm.rs`, `timestamp.rs`, `coordinates.rs`). Modify egui `crates/hxy/src/visualizers/*` onto it (shims/imports), `crates/hxy-templates/Cargo.toml` (+iced-x86, image? NO: image decoding (image crate) stays per-frontend? CHECK: `image::load_from_memory` is pure and both frontends need it -- move it: add `image` dep matching egui's version; texture upload stays per-frontend), root manifests + BUCK.

**Moves (verbatim; Color32->Rgba where colors appear):** VisualizerSpec/VisualizerKind/parse + tests, read_node_visualizer/lookup_visualizer/Inline, VisualizerTarget + a decoupled `collect_targets(instances: &[TemplateInstance]) -> Vec<VisualizerTarget>` (egui's takes &OpenFile -- refactor egui to call the decoupled version over file.templates; VisualizerKey stays as {instance: TemplateInstanceId, node: TemplateNodeIdx}), `read_field_bytes(source: &Arc<dyn HexSource>, offset, length)` decoupled likewise, bitmap {BitmapFormat, decode, parse_args, blake3_short_with_args}, digram/distribution grid builders returning `Vec<Rgba>` + dims (split the egui texture upload out; viridis/heat LUTs move), plot {Sample, parse_sample, samples}, sound {SampleFormat, downsample_for_plot, SoundCache data parts}, text decode fns, hex_viewer format_dump, disassembler {Bitness, disassemble_x86} (+iced-x86 dep), timestamp decode, coordinates resolve/clamp, image decode-to-rgba (fingerprint + `image::load_from_memory` -> (w, h, Vec<u8> rgba)).

**Gate:** root tests/clippy/buck green; egui behavior unchanged (visualizer_pipeline integration test is the oracle); gpui check green. Commits: `refactor(hxy): extract visualizer core into hxy-templates` (split per-module if large), `build: rebuckify for visualizer extraction` if BUCK changes.

---

### Task 2: gpui VisualizerPanel

**Files:** Create `gpui/hxy-gpui/src/panels/visualizer.rs`; modify panels/mod.rs (register VISUALIZER_PANEL_NAME), persist.rs (panel_kind OwningPathLeaf), workspace.rs (registry vec + open/focus/collect/dump_has helpers + is_known_panel_name + rebuild refresh), panels/file.rs (OpenVisualizer event -> workspace event FilePanelOpenVisualizer emitted; workspace opens/focuses the panel for that file and sets active target), palette (OpenVisualizer entry gated on targets nonzero), i18n ftl keys (visualizer-* -- reuse existing egui keys, add missing), gpui/Cargo.toml (image dep? gpui re-exports image? CHECK: gpui depends on image internally; RenderImage::new takes image::Frame -- use the image version gpui re-exports if public, else add matching dep).

**Panel structure (mirror EntropyPanel scaffolding + strings rebind):** owning_path + OpenFilePanels rebind; state: `active: Option<VisualizerKey>`, `caches: HashMap<VisualizerKey, KindCache>` (fingerprint + decoded artifacts: `Arc<RenderImage>` for texture kinds -- BGRA order, swap channels when building Frames; plot data vectors; listing strings), targets derived from bound FilePanel's templates each sync (observe the file panel entity; gc caches on target-set change mirroring egui gc()). Sub-tab strip of targets (compact Buttons, label = target.label). Body per kind:
- image/bitmap/digram/distribution: `img()` element from cached RenderImage; bitmap NEAREST (check img() sampling control; note deviation if unavailable); scrollable container, fit-to-width default.
- line/bar/chunk_entropy/sound: plot primitives exactly like panels/entropy.rs (ScaleLinear + shape::Line/Bar + PlotAxis/Grid/AxisText); chunk_entropy fixed Y domain [0, MAX_ENTROPY]; scatter: custom point paint via canvas (small filled quads at data positions, equal aspect).
- text/hex_viewer/disassembler: monospace scrollable read-only text (check gpui-component for a code/text view; else v_flex of Labels per line -- virtualized via uniform_list if long; disassembler listing can be large: use uniform_list).
- table: gpui-component Table of the node's direct children (reuse template_view column formatting helpers).
- coordinates: canvas painting world rect + point (theme colors).
- timestamp: label rows (decoded formats); three_d: placeholder label.
- Errors per kind -> i18n'd inline message (visualizer-read-error etc.).
**Wiring:** template panel visualizer marker + TemplateEvent::OpenVisualizer(idx) -> FilePanel emits event consumed by Workspace: open-or-focus VisualizerPanel for that file, set active = key for (active instance, idx). Rerun/instance-removal: targets resync + cache gc (observe file panel; the M4a rerun already notifies). Persistence: dump {path, active: [instance_idx? NO -- instance ids are not stable across restore; persist active as (source_path of template, node idx) or just drop active on restore -- EGUI persists only visualizer_open flag (state.rs OpenTabState.visualizer_open); mirror egui: persist only open/visibility via panel presence + owning path; active target recomputed}. panel_kind: OwningPathLeaf { log_label: "visualizer panel" }.

**Tests:** #[gpui::test] target derivation from a fixture template with visualize attrs; OpenVisualizer round-trip (event -> panel opens with active key); cache fingerprint invalidation on byte edit + rerun; decode smoke per pure kind already covered in Task 1 (hxy-templates tests) -- panel tests only cover wiring; BGRA channel order test (build RenderImage from a known 2x1 rgba fixture, assert stored frame bytes swapped).

Commits: `feat(hxy-gpui): visualizer panel` (+ `feat(hxy-gpui): visualizer palette and template wiring` if split needed).

---

### Task 3: Milestone gate

Full matrix (root + gpui tests/clippy/fmt, buck2 build, reindeer drift); manual smoke: run app, open PNG, run a .hexpat with [[hex::visualize]] attrs (write a tiny fixture pattern under user templates dir), verify image/plot/table kinds render and follow re-runs; milestone-level fresh-subagent adversarial review (cross-task + CLAUDE.md sweep); record deferrals; spec status note M4b done.
