# M4a: GPUI Template Runner Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bring the GPUI app to template parity: run 010/.bt, ImHex/.hexpat, and WASM-plugin templates against the active file (whole-file or selection), render the results tree in a per-file bottom panel with field tints/hover/selection sync in the hex view, template library picker in the palette, persistence + re-run on restore, and re-run-on-edit.

**Architecture:** Same M0-M3 pattern. A new desktop-only shared crate `hxy-templates` receives (a) verbatim moves of the egui-free glue already in `crates/hxy/src/templates/` (builtin runtime adapters, library/include sandbox, runner-pure helpers) and (b) the framework-agnostic half of `crates/hxy/src/panels/template.rs` (state model, tree flattening, leaf/color model, value formatting, breadcrumbs) decoupled from `egui::Color32` via a plain `Rgba` newtype. The egui app is refactored onto the shared crate with behavior unchanged. The GPUI side then adds a runner (gpui background executor instead of egui_inbox), per-file template state on `FilePanel`, a bottom template table UI, hex-view tint/hover/selection integration, palette modes, and persistence.

**Why a new crate, not hxy-panels:** templates require `hxy-plugin-host` (wasmtime), which cannot build for wasm32; `hxy-panels` is compiled into the egui wasm build. `hxy-templates` is desktop-only by construction.

**Tech Stack:** gpui =0.2.2, gpui-component =0.5.1 (Table, virtualized rows, ContextMenu, ColorPicker if present else swatch buttons), hxy-plugin-host (TemplateRuntime/ParsedTemplate/ResultTree), hxy-010-lang, hxy-imhex-lang, blake3, jiff.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-24-gpui-port-design.md` (M4 scope).
- Nested workspace layout: GPUI crates in `gpui/` (cargo from `gpui/`); shared crates in `crates/` (root workspace). BOTH green (tests + clippy zero warnings + `cargo fmt --check` clean in gpui/) at every task end. Root builds also verified with `nix develop --command buck2 build //:hxy` after Cargo.toml changes (run `nix develop --command reindeer buckify` and commit BUCK when the root dep graph changes).
- jj only; `-m` messages; conventional commits; NO AI attribution; ASCII only; no separator comments; no historical framing. Leave `@` described-and-complete at task end (next task stacks with `jj new`).
- Typed thiserror errors; newtypes/enums over bools/primitives; scrutinize new `unwrap_or*`.
- i18n: every user-visible hxy-gpui string through `hxy_i18n::t`/`t_args`. The egui template panel hardcodes its column headers and labels in English (template.rs) -- do NOT copy that; add new `template-*` keys to `crates/hxy-i18n/translations/en-US/main.ftl` for the gpui panel. Reuse existing `palette-*template*`, `toast-template-*`, `visualizer-row-tooltip` keys where they exist (grep first).
- Extractions are verbatim moves with tests; egui app behavior unchanged; re-export shims; the moved/stayed boundary = "compiles without app types and without egui" (Color32 boundary handled by the Rgba newtype + `From` impls on the egui side).
- Desktop-only: everything in this plan is `not(target_arch = "wasm32")` on the egui side already; `hxy-templates` and all gpui code are desktop-only.
- Every task: fresh-subagent review gate; fix loops per SDD skill.
- Reference architecture: all file:line citations below were verified 2026-08-20; where a citation is stale, the current source wins.

### Key reference points (read before any task)

- egui template subsystem: `crates/hxy/src/templates/{builtin.rs,library.rs,runner.rs,patterns_fetch.rs}`, `crates/hxy/src/panels/template.rs` (2012 lines; ~half pure logic), `crates/hxy/src/files/mod.rs:386-560` (TemplateInstanceId, TemplateInstance, TemplateRunInstance, TemplateRunOutcome, TemplateNodeIdx, TemplateArrayId, TemplateState), `crates/hxy/src/view/hex_body.rs:24-245` (tint/hover/breadcrumb wiring), `crates/hxy/src/app/mod.rs:2270-2440` (render_template_panel + apply_template_event reducer + select_template_node), `crates/hxy/src/app/desktop.rs:552,1018-1240` (template_runtime_for, rerun cascade, restore), `crates/hxy/src/state.rs:31-80` (PersistedTemplateInstance).
- Data model (WIT, reuse verbatim): `crates/hxy-plugin-host/src/template.rs` -- ResultTree, Node (flat list, parent indices), Span, NodeType, ScalarKind, Value, DisplayHint, DeferredArray, Diagnostic, attribute constants (BITFIELD_BITS_ATTR, ENDIAN_ATTR, COLOR_ATTR, BG_COLOR_ATTR, COMMENT_ATTR, FORMAT_ATTR, NAME_ATTR, VISUALIZE_ATTR, INLINE_VISUALIZE_ATTR, VISUALIZE_ARG_SEP), traits TemplateRuntime/ParsedTemplate, WasmTemplateRuntime; `registry.rs:148,194` plugin loading.
- GPUI app architecture: `gpui/hxy-gpui/src/workspace.rs` (Workspace root, dock idiom, globals ActiveHexPane/OpenFilePanels, reconcile, add-panel recipe), `gpui/hxy-gpui/src/panels/file.rs` (FilePanel owns Entity<HexPane> + SearchBar below pane -- the template panel mounts the same way), `gpui/hxy-gpui/src/panels/strings.rs` (background compute pattern: running/pending_rerun guard, cx.spawn + cx.background_spawn, Task kept in field), `gpui/hxy-gpui/src/persist.rs` (panel_kind pruning), `gpui/hxy-gpui/src/palette/{modes.rs,apply.rs}` (PaletteMode/PaletteAction/build_entries), `gpui/hxy-view-gpui/src/pane.rs` (set_byte_styler :193, set_hover_span :172, ByteStyleOverride {bg,fg} :56, sync_pending_scroll :520), `gpui/hxy-view-gpui/src/paint.rs` (paint_styler_tints :565, PaintColors).

---

### Task 1: Create hxy-templates crate; move egui-free glue verbatim

**Files:**
- Create: `crates/hxy-templates/Cargo.toml`, `crates/hxy-templates/src/lib.rs`, `crates/hxy-templates/src/builtin.rs`, `crates/hxy-templates/src/library.rs`, `crates/hxy-templates/src/run.rs`
- Modify: root `Cargo.toml` (workspace member + `[workspace.dependencies] hxy-templates = { path = "crates/hxy-templates", version = "0.5.0" }`), `gpui/Cargo.toml` (workspace dep, added in Task 3 but harmless now), `crates/hxy/Cargo.toml` (dep on hxy-templates), `crates/hxy/src/templates/builtin.rs` -> shim (`pub use hxy_templates::builtin::*;`), `crates/hxy/src/templates/library.rs` -> shim, `crates/hxy/src/templates/runner.rs` (import moved helpers)
- BUCK: `nix develop --command reindeer buckify` after root manifest changes; commit regenerated BUCK.

**Moves (verbatim, with their unit tests):**
- `builtin.rs` (494 lines, egui-free): `builtins()`, `Bt010Runtime`, `ImHexRuntime`, `HexSourceShim`, all `convert_*` fns, `build_default_resolver`, `first_nonempty_attr`.
- `library.rs` (826 lines, egui-free): `TemplateLibrary`, `TemplateEntry`, `load_from_dirs`, `suggest`, `rank_entries`, include sandbox (`parse_include_directives`, `collect_include_closure`, `resolve_within`, `install_template_with_deps`, `expand_includes`, `list_installed_templates`), `DETECTION_WINDOW`.
- From `runner.rs` into `run.rs` (pure parts only): `SubrangeSource` (runner.rs:373), `OffsetAdjustedTemplate` (runner.rs:405), `adjust_tree`/`adjust_node_span`/`adjust_diagnostic`, `fingerprint_template_source` (runner.rs:314). The egui-coupled orchestration (`run_template_from_path`, `drain_template_runs`, UiInbox plumbing) STAYS in `crates/hxy/src/templates/runner.rs` and now imports these.

**Crate deps:** hxy-core, hxy-plugin-host, hxy-010-lang, hxy-imhex-lang, blake3, thiserror, tracing, jiff (whatever the moved code actually uses; no egui, no gpui). `version.workspace = true` etc. matching sibling crates; `publish = false` is NOT set on other crates -- match `crates/hxy-panels/Cargo.toml` package style.

**Interfaces (Produces):** `hxy_templates::builtin::builtins() -> Vec<Arc<dyn TemplateRuntime>>`; `hxy_templates::library::{TemplateLibrary, TemplateEntry, rank_entries, install_template_with_deps, expand_includes, list_installed_templates}`; `hxy_templates::run::{SubrangeSource, OffsetAdjustedTemplate, fingerprint_template_source}`.

**Steps:** move builtin -> shim -> root `cargo test --workspace` green -> commit `refactor(hxy): move builtin template runtimes into hxy-templates`; move library -> shim -> test -> commit `refactor(hxy): move template library into hxy-templates`; move runner helpers -> imports -> test -> commit `refactor(hxy): move template run helpers into hxy-templates`; buckify + commit `build: register hxy-templates with reindeer` if BUCK changed. Final: clippy root workspace; `nix develop --command buck2 build //:hxy`.

---

### Task 2: Extract framework-agnostic template panel core into hxy-templates::state

**Files:**
- Create: `crates/hxy-templates/src/state.rs`, `crates/hxy-templates/src/format.rs`, `crates/hxy-templates/src/breadcrumb.rs`, `crates/hxy-templates/src/color.rs`
- Modify: `crates/hxy/src/panels/template.rs` (refactor onto the extracted core; egui rendering + egui_table delegate stay), `crates/hxy/src/files/mod.rs` (TemplateState fields switch `egui::Color32` -> `hxy_templates::color::Rgba`; egui converts at render boundary), `crates/hxy/src/state.rs` (PersistedTemplateInstance color type -> Rgba with serde compat -- keep the on-disk JSON shape identical: Color32 serializes as [r,g,b,a]; implement serde on Rgba to match, with a unit test round-tripping the old format), `crates/hxy/src/visualizers/table.rs` + `mod.rs` (imports for moved format_value), `crates/hxy/src/settings/mod.rs` (NumericFormat/NumericBase/TemplateValueFormats MOVE to `hxy-core::format` -- they are tiny pure enums used by shared formatting; egui settings re-export), `crates/hxy/src/view/format.rs` (format_offset moves to hxy-core::format alongside; shim re-export)
- Test: extracted logic keeps its tests in hxy-templates; new Rgba serde-compat test; egui side keeps its UI tests.

**`color.rs` (new):** `#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub struct Rgba { pub r: u8, pub g: u8, pub b: u8, pub a: u8 }` with `pub const fn rgb(r,g,b)`, `from_argb_u32(0xAARRGGBB)`, serde as `[r,g,b,a]` tuple (Color32-wire-compatible). egui side: `impl From<Rgba> for egui::Color32` and back, living in `crates/hxy/src/panels/template.rs` (or a small egui-side util module) -- NOT in hxy-templates.

**Moves into `state.rs` (from crates/hxy/src/panels/template.rs and files/mod.rs):**
- Types: `TemplateState` (parsed, tree, expanded_arrays, collapsed, hovered_node, selected_node, leaf_boundaries, leaf_colors, leaf_node_indices, leaf_slot_by_node, node_color_overrides, show_colors, byte_palette_override -- colors as Rgba), `TemplateNodeIdx`, `TemplateArrayId`, `TemplateEvent` (all 16 variants; SetColor carries Rgba), `RowKind`.
- Construction/mutation: `new_state_from`, `new_state`, `error_state`, `expand_array` (MAX_INITIAL=512), `toggle_collapse`, `recompute_leaf_colors`.
- Tree logic: `children_by_parent`, `build_children_index`, `build_visible`, `emit_node`, `visible_node_indices`, `initial_collapsed`, `all_children_scalar`.
- Leaf/color model: `collect_leaves`, `resolve_leaf_colors` (override > hxy_color attr > fallback_leaf_color golden-angle cycle), `fallback_leaf_color`, `build_byte_palette_override`, `parse_color_attr`, `parse_hex_color`.
- `format.rs`: `format_value`, `format_unsigned_int`, `format_signed_int`, `pick_value_base`, `flip_if`, `quote_string_preview`, `quote_bytes_preview`, `bytes_as_string_preview`, `looks_like_text`, `decode_scalar_bytes`, `scalar_kind_width` (consume `hxy_core::format::{NumericFormat, NumericBase, TemplateValueFormats}` after the move).
- `breadcrumb.rs`: `BreadcrumbDetail`, `breadcrumb_for_offset`, `format_leaf_line`, `array_element_row`, `format_node_value`.

**TemplateInstance move:** `TemplateInstanceId`, `TemplateInstance` (id, source_path, display_name, range, source_fingerprint, state) also move to `hxy_templates::state` so both frontends share them. `TemplateRunInstance`/`TemplateRun`/`TemplateRunOutcome` STAY egui-side (they embed UiInbox); the gpui side builds its own run tracking in Task 3.

**Steps:** per-module move -> egui refactor -> root `cargo test --workspace` green (count baseline before/after) -> commits: `refactor(hxy-core): host shared numeric format types`, `refactor(hxy): extract template state core into hxy-templates`, `refactor(hxy): extract template value formatting into hxy-templates`, `refactor(hxy): extract template breadcrumbs into hxy-templates`. Final: clippy; buck build; verify egui app runs a .bt template unchanged (manual or existing tests).

---

### Task 3: GPUI template runner + per-file template state

**Files:**
- Create: `gpui/hxy-gpui/src/templates.rs`
- Modify: `gpui/Cargo.toml` + `gpui/hxy-gpui/Cargo.toml` (deps: hxy-templates, hxy-plugin-host workspace entries), `gpui/hxy-gpui/src/panels/file.rs` (template state storage), `gpui/hxy-gpui/src/main.rs` (runtime registry init)

**Interfaces (Produces):**
- `pub struct TemplateRuntimes(Vec<Arc<dyn TemplateRuntime>>)` -- gpui App global; built at startup: `hxy_templates::builtin::builtins()` prepended by WASM plugins from `user_template_plugins_dir()` (port `load_user_template_plugins` from `crates/hxy/src/app/mod.rs:2799` and dir helpers :2763-2772 -- share the SAME dirs so both apps see the same plugins/templates). `pub fn runtime_for(&self, ext: &str) -> Option<Arc<dyn TemplateRuntime>>` (case-insensitive, first wins; port desktop.rs:552).
- `pub struct TemplateRunHandle { pub id: TemplateInstanceId, pub display_name: String, pub started: jiff::Timestamp, task: Task<()> }` on FilePanel.
- `FilePanel` gains: `templates: Vec<TemplateInstance>`, `templates_running: Vec<TemplateRunHandle>`, `active_template: Option<TemplateInstanceId>`, `next_template_instance_id: u64`, `last_template_path: Option<PathBuf>`, `template_panel_visible: bool`, plus accessors mirroring egui `files/mod.rs:688` (`fresh_template_instance_id`, `active_template()`, `active_template_mut()`, `upsert_instance`).
- `pub fn run_template(file: &mut FilePanel, path: PathBuf, range: Option<ByteRange>, restore: RestoreContext, window, cx)` in `templates.rs`: ports runner.rs:48 flow -- extension -> runtime_for (error instance on miss), bound_range check, `expand_includes` when under `user_templates_dir()`, `fingerprint_template_source`, override reconcile, `SubrangeSource` wrap for slices, then `cx.spawn` + `cx.background_spawn(move || { runtime.parse(...); OffsetAdjustedTemplate::wrap(...).execute(&[]) })`, applying the outcome in `this.update(...)` via `new_state_from`/`error_state` + `upsert_instance`. `pub struct RestoreContext { pub expected_fingerprint: Option<[u8; 32]>, pub overrides: HashMap<u32, Rgba> }` (port runner.rs:40 with Rgba).
- Errors and diagnostics surface as toasts (`window.push_notification`) + stored diagnostics in state (rendered in Task 4).

**Note:** no counterpart of egui's per-frame `drain_template_runs` -- gpui delivers results by entity update from the spawned task (strings.rs::run pattern with running/pending guard per instance).

**Steps:** deps -> types -> run_template with a `#[gpui::test]` driving a tiny inline .bt template end-to-end against a MemorySource-backed FilePanel (assert tree lands, spans absolute, error path yields error_state) -> `cd gpui && cargo test -p hxy-gpui` green -> clippy/fmt -> commit `feat(hxy-gpui): template runtime registry and background runner`.

---

### Task 4: Template results panel UI (bottom of FilePanel)

**Files:**
- Create: `gpui/hxy-gpui/src/panels/template_view.rs`
- Modify: `gpui/hxy-gpui/src/panels/file.rs` (mount below pane like SearchBar; route events), `gpui/hxy-gpui/src/panels/mod.rs` (module decl only -- NOT a registered dock panel; it lives inside FilePanel), `crates/hxy-i18n/translations/en-US/main.ftl` (+ other locale files present -- mirror keys)

**UI (gpui-component native widgets, information density matching egui template.rs:100-358):**
- Header row: panel title (i18n `template-panel-title`), colors toggle (Button compact, selected = show_colors, i18n `template-toggle-colors`), close button (i18n `template-close`).
- Tab strip: one compact Button per instance (display_name + range suffix when sliced, close X, spinner while running -- port render_tab_strip semantics, template.rs:1278).
- Diagnostics section: collapsible list of Diagnostic {severity icon, message, optional offset link that jumps the hex view} (i18n `template-diagnostics`).
- Results table: gpui-component `Table` + `TableDelegate` (virtualized; EntropyPanel/strings.rs show Table usage) with 7 columns: color swatch, name (indented by depth, expand/collapse chevron on parents), type, start, end, length, value. Rows from `build_visible(state, children)` -> `Vec<RowKind>`; formatting via `hxy_templates::format::format_value` and `hxy_core::format::format_offset`. Color swatch click opens a small color-pick popover (gpui-component ColorPicker if available in 0.5.1, else a fixed 12-swatch palette menu) -> `TemplateEvent::SetColor`; shift-click -> ResetColor. Row hover -> Hover(idx); click -> Select(idx); chevron -> ToggleCollapse; deferred arrays render an Expand button (ExpandArray, count MAX_INITIAL). Context menu (gpui-component ContextMenu): copy value / copy struct / save bytes to file (rfd) -- port template.rs row_context_menu actions. Keyboard: up/down -> MoveSelection, left/right -> Collapse/ExpandSelected when table focused (key_context "TemplateTable").
- Comment marker: INFO icon + tooltip for COMMENT_ATTR; visualizer marker icon for nodes with VISUALIZE_ATTR (fires OpenVisualizer -- wired fully in M4b; until then it toggles pending state only).
- All strings via i18n; add `template-*` keys (title, columns name/type/start/end/length/value, diagnostics, running, expand, copy-value, copy-struct, save-bytes, no-template, close, toggle-colors).

**Event reducer:** `FilePanel::apply_template_event(event, window, cx)` ports app/mod.rs:2300 -- Hover -> pane.set_hover_span(span) (template beats strings hover -- strings.rs already clears via on_removed; establish priority: template hover set last wins, clear on None); Select -> editor.set_selection + set_scroll_to_byte + pane.sync_pending_scroll (port select_template_node app/mod.rs:2406); SetColor/ResetColor -> overrides + recompute_leaf_colors + restyle; RemoveInstance/SetActive/HidePanel/ToggleColors -> state flips; Copy -> cx.write_to_clipboard; SaveBytes -> rfd save dialog + fs write with toast on error.

**Steps:** panel skeleton + table rendering test (`#[gpui::test]` building a small ResultTree and asserting visible-row derivation + event emission on simulated clicks where the harness allows; logic-level tests for anything the harness can't click) -> reducer -> keyboard nav -> i18n keys -> `cargo test -p hxy-gpui` + clippy/fmt green -> commit `feat(hxy-gpui): template results panel` (split UI/reducer commits if large).

---

### Task 5: Hex-view integration: tints, palette override, breadcrumbs

**Files:**
- Modify: `gpui/hxy-gpui/src/panels/file.rs` (styler composition), `gpui/hxy-view-gpui/src/pane.rs` + `paint.rs` (custom byte-value palette support), `gpui/hxy-gpui/src/panels/template_view.rs` (breadcrumb strip)

**Byte styler composition (port hex_body.rs:68-160 semantics):** when active template exists and show_colors: closure captures sorted `leaf_boundaries` + `leaf_colors`; `partition_point` lookup; bg tint at 0.45 alpha (Hsla via `Rgba -> gpui::Hsla` helper; add `pub fn rgba_to_hsla(Rgba) -> Hsla` in templates.rs); modified/patched bytes keep their existing style (the current styler used for patch marks wins -- compose: patch-style first, template tint only when patch returns none). Selection/hover bands already win in paint_styler_tints.
- **Palette override:** `byte_palette_override` needs an equivalent of egui `HighlightPalette::Custom`. Add `HexPane::set_value_palette(Option<Arc<[Hsla; 256]>>, cx)` in pane.rs + honor it in paint_row_text byte_color resolution (paint.rs; default palette when None). Follow the ByteStyleOverride precedent (setter + snapshot field + paint consumption + cleared by set_source).
- **Hover span:** already wired via reducer (Task 4).
- **Breadcrumbs:** egui shows an always-open tooltip at the hovered offset (hex_body.rs:198-245). gpui port: a single-line breadcrumb readout at the top of the template panel (joined with " > ", monospace, Full detail on alt -- read modifiers from window) driven by the pane's hovered offset. HexPane exposes hover position? If not: add `HexPane::hovered_offset() -> Option<ByteOffset>` fed from existing mouse-move handling (pane.rs tracks mouse for selection; extend to record hover cell via geometry hit test). Emit nothing new in paint.

**Steps:** palette setter + paint honor + unit/paint-smoke test -> styler composition (test: styler returns template tint on untouched byte, patch style on patched byte) -> hovered_offset + breadcrumb strip -> both workspaces green -> commits: `feat(hxy-view-gpui): custom byte-value palette`, `feat(hxy-gpui): template field tints and breadcrumbs in the hex view`.

---

### Task 6: Palette modes, library, suggestions, persistence, re-run

**Files:**
- Modify: `gpui/hxy-gpui/src/palette/modes.rs` + `apply.rs` + `mod.rs`, `gpui/hxy-gpui/src/workspace.rs` (library global, suggestion toasts, pending runs), `gpui/hxy-gpui/src/panels/file.rs` (dump/restore, byte-change cascade), `gpui/hxy-gpui/src/templates.rs` (patterns fetch port), `crates/hxy-i18n/translations/en-US/main.ftl` (keys if missing)

**Palette:** new `PaletteMode::Templates` and `PaletteMode::TemplatesAtSelection` (parent = Main; hint keys `palette-hint-templates`/`-at-selection`); entries from `TemplateLibrary::rank_entries(ext, head_bytes)` (library global loaded at startup from `user_templates_dir()` + fetched imhex dir; port app/mod.rs:2782) -> `PaletteAction::RunTemplate { path, range: Option<ByteRange> }`, plus `PaletteAction::{InstallTemplate, UninstallTemplate(path), JumpNextField, JumpPrevField, RunTemplateDialog}`. Main-mode entries: "Run template..." (opens Templates mode), "Run template on selection..." (gated on selection), jump next/prev field (gated on `TemplateCtx.field_count > 0`; port entries.rs:89). apply.rs routes: RunTemplate -> active FilePanel run_template; InstallTemplate -> rfd pick + `install_template_with_deps` + toast with InstallReport summary; Uninstall mode lists `list_installed_templates`; jump fields -> port apply.rs:112 `jump_to_template_field` (leaf_boundaries binary search from cursor, wraps).
- **Suggestions:** on file open, `TemplateLibrary::suggest(ext, head DETECTION_WINDOW bytes)` -> notification toast "Run <name>?" with run/dismiss buttons (i18n `toast-template-*` existing keys) -> run_template on accept (port desktop.rs:1059 + toasts.rs flow as a gpui Notification with action button; gpui-component Notification supports buttons -- verify, else render a transient bar in FilePanel).
- **Persistence:** FilePanel::dump gains `templates: Vec<PersistedTemplateInstance>` JSON (source_path, range, fingerprint hex, node_color_overrides as {idx: [r,g,b,a]}), `active_template_idx`, `template_panel_visible`. restore: after source loads, re-fire each with RestoreContext (port desktop.rs:1203-1240; skip while Loading -- FilePanel restore path is synchronous read, so fire directly after bind), then set active idx. persist.rs: no new panel_kind (template panel is inside FilePanel).
- **Re-run-on-edit:** FilePanel already knows edits (dirty tracking / editor changes feed strings recompute). Add the same debounced hook: on editor byte-change (reuse the existing change-notification path that strings/entropy use -- the OpenFilePanels observers recompute on reload; for edits, FilePanel::render observes editor dirty revision (add a `revision: u64` bump on splice in hxy-editor if none exists -- check first; egui uses drain_byte_change_cascade)). On change: snapshot instances (path, range, RestoreContext with fingerprint+overrides), clear, re-fire (port desktop.rs:1018). Skip when a run is in flight.
- **ImHex patterns fetch:** port patterns_fetch.rs run_fetch to a plain function executed via cx.background_spawn with progress delivered by entity updates; palette entry "Fetch ImHex patterns" + progress/success/failure toasts; reuse `install_dir`/`fingerprint_existing`.

**Steps:** palette modes + tests (build_entries unit tests mirror modes.rs existing tests) -> suggestions -> persistence round-trip test (dump -> restore re-runs template with overrides preserved) -> re-run cascade test (splice -> instance re-executes, overrides survive via fingerprint match) -> fetch -> both workspaces green -> commits: `feat(hxy-gpui): template palette modes and library`, `feat(hxy-gpui): template suggestions and persistence`, `feat(hxy-gpui): template re-run on edit`, `feat(hxy-gpui): imhex patterns fetch`.

---

### Task 7: Milestone gate

- Full: root `cargo test --workspace`; `cd gpui && cargo test && cargo clippy --all-targets` (zero warnings) + `cargo fmt --check`; `nix develop --command buck2 build //:hxy`; `nix develop --command reindeer buckify` drift check.
- Manual smoke on macOS: run gpui app, open a PNG, run an ImHex pattern and a .bt template, verify tints/hover/selection/breadcrumbs/colors toggle/persistence across restart, run-on-selection, re-run after edit.
- Adversarial review by a fresh subagent against global CLAUDE.md + this plan; fix findings.
- Update `docs/superpowers/specs/2026-07-24-gpui-port-design.md` status notes if M4 scope shifted.
- Commit any fixes; leave `@` clean.
