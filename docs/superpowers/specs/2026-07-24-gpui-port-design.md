# GPUI port of hxy: design

Date: 2026-07-24
Status: implemented (M0-M4 complete as of 2026-08-21). The GPUI app
reaches feature parity with the egui app across all subsystems; the
remaining differences are catalogued in "Known deviations from the
egui app (as of M4)" below. See docs/superpowers/plans/2026-08-20-m4f-parity-matrix.md
for the full feature-by-feature matrix.

## Goal

Port hxy (egui) to GPUI with gpui-component so the two UI frameworks can be
compared side by side in a real hex editor. Requirements:

1. Functional equivalence
2. Feature equivalence (full parity, including docking and pane-picker)
3. Similar UX/UI: same layout, panels, shortcuts, and information density,
   but idiomatic gpui-component theming and widgets (not a pixel clone)

Non-goals:

- Web/wasm target for the GPUI app (GPUI does not support wasm)
- Pixel-faithful visual cloning of the egui app
- New features not present in the egui app

## Context

The workspace splits roughly in half:

- Framework-agnostic, reused as-is (~42k LOC): hxy-core, hxy-vfs, hxy-i18n,
  hxy-calculator, hxy-010-lang, hxy-imhex-lang, hxy-plugin-host,
  hxy-plugin-api.
- egui-coupled, the port surface (~42k LOC): crates/hxy (app, ~35k),
  hxy-view (~6k), egui-palette, egui_minimap, egui_dock_picker.

Work happens in a new jj workspace; new crates join this Cargo workspace.

## Crate architecture

| Crate | Role |
|---|---|
| hxy-editor (new) | Extracted from hxy-view: HexEditor state (source, selection, suture patch, undo/redo, scroll model, typing/edit modes), vim state machine, and an abstract input-intent layer (framework-neutral input events in, editor mutations out). No egui, no gpui. |
| hxy-view (existing) | Becomes the egui rendering and input-binding layer over hxy-editor. Public API preserved where practical; existing egui_kittest interaction tests keep passing. |
| hxy-view-gpui (new) | GPUI rendering and input-binding over hxy-editor: address gutter, hex/ASCII grids, per-byte styling, field overlays, minimap. Custom-painted monospace grid. Prior art: rusthex, gpui_hexeditor. |
| gpui_dock_picker (new) | Reusable vimium-style pane-picker overlay for gpui-component DockArea (equivalent of egui_dock_picker, which gpui-component lacks). |
| hxy-gpui (new) | The app: docking workspace, all tabs/panels, command palette, menus, dialogs, IPC, settings. Reuses the framework-agnostic crates unchanged. |

Command-palette fuzzy-filter/entry logic gets the same split where practical:
shared logic extracted, thin per-framework frontends (gpui-component ships a
fuzzy Picker to build on).

## Dependencies and platform

- gpui and gpui-component from crates.io (gpui 0.2.x, gpui-component
  0.5.x), version-pinned in the gpui workspace Cargo.toml. The git route
  upstream docs recommend is unusable without forking: gpui-component's
  manifest pulls gpui from zed's default branch with no rev, Cargo cannot
  patch two revs of the same git source, and zed tip requires a newer
  Rust toolchain than the repo pins. Both crates are pre-1.0; version
  pins insulate against API churn.
- The GPUI crates live in a nested Cargo workspace (gpui/ directory with
  its own Cargo.toml and lockfile), path-depending on the shared crates in
  crates/. Forced by hard dependency conflicts between the zed git tree
  and the egui stack (wgpu minor pins, core-foundation exact pins,
  accesskit) that make a single shared lockfile unresolvable. The egui
  and GPUI apps therefore build independently; shared crates compile in
  both lockfiles.
- Desktop only. macOS is the tested target; Linux/Windows should compile
  (GPUI supports both) but do not gate milestones.

## Docking and pane-picker equivalence

Verified in gpui-component source (crates/ui/src/dock): DockArea/DockItem
(recursive Split/Tabs/Panel/Tiles tree), drag-and-drop tab docking with
edge-drop splitting, tab reordering, runtime add/close, layout persistence
via dump()/load() with PanelRegistry, and DockEvent::LayoutChanged for
persistence triggers. This covers egui_dock parity for the main workspace.

To build or verify:

1. Nested workspace docks (hxy's Tab::Workspace owns an inner DockState):
   unverified upstream. Nothing structurally prevents a Panel rendering its
   own DockArea, but no example exists. M2 opens with a spike; fallback is
   serializing the inner dock state through our own PanelState payload if
   persistence is the sticking point.
2. gpui_dock_picker: overlay rendering keycap labels over each visible
   panel; keypress focuses that pane. Feature-equivalent to egui_dock_picker.

## App architecture in GPUI

- The immediate-mode HxyApp + per-frame TabViewer becomes a retained root
  workspace entity owning the DockArea. Each Tab variant becomes a
  gpui-component Panel view over the same shared state types.
- egui_inbox channels (IPC, VFS open, async template/plugin results) become
  plain channels polled via GPUI's executor (cx.spawn + entity updates).
  The IPC socket and rkyv framing code is reused as-is.
- Keyboard shortcuts become GPUI actions + keymap. In-editor keys flow
  through hxy-editor's input layer, so vim behavior is identical in both
  apps by construction.
- All UI strings go through hxy_i18n::t / t_args (existing rule).
- Theming: follow the system dark/light preference at startup and on
  runtime changes, matching the egui app's behavior. Default theme choice
  mirrors the egui app (its default dark and light looks map to the
  closest gpui-component built-in theme pair); everything else stays
  idiomatic gpui-component.
- Visualizers: gpui-component charts replace egui_plot; bitmap, digram,
  image, and sound visualizers use GPUI canvas/paint primitives.

## Milestones

Each milestone is planned and implemented as its own cycle (plan, execute,
review, commit); this spec governs all of them.

- M0: extract hxy-editor (and palette logic); refactor the egui side onto
  it. Gate: existing tests and egui_kittest interaction tests green; egui
  app behavior unchanged.
- M1: hxy-view-gpui plus a minimal shell (open file, hex/ASCII grid,
  keyboard nav including vim, selection, drag-select, editing, minimap).
  First feel-comparison possible here.
- M2: DockArea workspace, tabs, nested-dock spike, gpui_dock_picker,
  command palette with calculator, search, goto, data inspector, menus,
  toasts.
- M3: strings, entropy, checksums, compare/diff, VFS browser (including
  workspace tabs via the nested-dock wrapper), file watching, snapshots,
  save, global search.
- M4 is delivered in sub-milestones: M4a (template runner, done; plan
  2026-08-20-m4a-gpui-templates.md), M4b (visualizers, done; plan
  2026-08-20-m4b-gpui-visualizers.md), M4e (settings, done; plan
  2026-08-20-m4e-gpui-settings.md; shared hxy.db with the egui app),
  M4g (theme + icon parity, done; plan 2026-08-20-m4g-gpui-theme-icons.md;
  brand theme JSON, six-class byte palette, embedded icon assets), M4c
  (plugins/mounts, done; plan 2026-08-20-m4c-gpui-plugins-mounts.md;
  grants shared via hxy.db, palette commands, VFS mounts, plugins panel),
  M4d (single-instance IPC + CLI open + macOS open-with, done; plan
  2026-08-20-m4d-gpui-ipc.md; shared hxy-ipc crate, gpui-native
  on_open_urls; NSServices right-click provider not ported), and M4f
  (console tab, i18n sweep incl. gpui system-locale init, final parity
  audit, done; plan 2026-08-20-m4f-gpui-console-i18n-audit.md). All M4
  sub-milestones complete.
- M4: template runner (010/ImHex), visualizers (they are driven by
  template visualize attributes and have no data source before templates
  exist), plugins and mounts, IPC single-instance, settings persistence,
  i18n sweep, welcome and console tabs.

## Verification and review

Every milestone ends with:

1. cargo clippy and the full test suite green.
2. An adversarial review by a fresh subagent against the global CLAUDE.md
   rules; findings fixed.
3. Incremental jj commits (conventional commit style, no AI attribution).

Visual comparison: egui_kittest screenshots of egui components serve as the
reference; GPUI has no render-to-image API, so the GPUI side uses OS-level
screenshots of a real window plus gpui::test / VisualTestContext for
interaction-level tests.

## Risks

- gpui API churn: mitigated by pinned revs.
- Nested dock-in-a-tab unproven: spiked at the start of M2 with a
  serialization fallback.
- No GPUI pixel-snapshot testing: OS screenshots instead.
- First build of the zed dependency tree is heavy (build time, lockfile
  growth).

## Known deviations from the egui app (as of M4)

The port reached functional + feature equivalence; these are the honest,
deliberate differences. The full row-by-row audit is
`docs/superpowers/plans/2026-08-20-m4f-parity-matrix.md` (parity 78,
deviation 24, gap 14). Each deviation below is one line: what, then why.

Settings (recorded in `gpui/hxy-gpui/src/settings.rs` module doc):

- `zoom_factor` not applied: gpui-component has no clean global scale knob and
  rem scaling would miss the custom-painted mono hex grid.
- `check_for_updates` not applied: no update checker exists in either frontend
  (a placeholder in egui too).
- `language` unapplied: dead in egui as well (no UI, never applied).
- `byte_cache_limit_mib` not applied: gpui opens whole files into
  `MemorySource` and constructs no `hxy_core::ByteCache`.
- `imhex_patterns` fetch state: tracked by the palette flow directly; the
  shared blob round-trips untouched.
- Global `hex_columns` change clobbers a palette-set per-pane column count:
  the gpui pane has no per-tab `hex_columns_override` slot yet.
- Nested (workspace-host) file panes read settings at construction only: live
  changes reach top-level and compare panes (matching the M3 nested-dock scope).
- `address_separator_enabled/_char` not honored (`hxy-view-gpui/paint.rs`): the
  grouped-address formatter lives in the egui-only hxy-view, not shared code.

Rendering / theme:

- Selection tint reads as a translucent wash, not egui's opaque fill
  (`theme.rs`): gpui-component clamps selection alpha to ~0.3.
- No violet active-tab outline (`theme.rs`): gpui-component has no per-tab
  border theme key; inactive tabs are dimmed by text color instead.
- Platform default mono font, not an embedded one (`theme.rs`): the egui app
  embeds its own; gpui uses `mono_font_family`.
- Texture visualizers render with the default bilinear filter and no
  scroll-at-native-size (`panels/visualizer.rs`): gpui 0.2.2's `img()` has no
  NEAREST sampler or native-size scroll affordance.
- ChunkEntropy X axis shows hex-offset ticks without captions
  (`panels/visualizer.rs`): matches the shell's EntropyPanel chart rather than
  egui's decimal ticks and axis labels.

Behavior / surface:

- Console does not autoscroll to a fresh entry (`panels/console_view.rs`):
  entries render oldest-first, newest at the bottom; the user scrolls to it.
- Plugin command palette rows carry a leading puzzle-piece icon token
  (`palette/modes.rs`, `assets.rs`) rather than an inline glyph as in egui.
- Native menu items are always enabled (`menu.rs`): every handler is bound on
  the root div, so `is_action_available` always reports available; handlers
  no-op gracefully. egui greys items per frame.
- Global-search results sort by insertion, not egui's `FileId`-ascending order
  (`panels/global_search.rs`): gpui has no orderable file id.
- Status bar omits egui's click-to-toggle offset base, copyable hover-value
  readout, click-to-copy value labels, tab-focus chip, and click-to-toggle
  watch chip (`status.rs`): file name, offset/selection, vim mode, dirty
  marker, and the lock toggle are ported; the rest are small egui affordances
  not re-created.
- Per-target copy commands (caret offset/address, selection range/length, file
  length) collapse into the palette `CopySelection` formats plus menu copy
  bytes/hex; caret- and file-length copies are not individually exposed.
- Watch prefs, global column count, plugin uninstall, and dock split/merge/
  move-tab verbs live on their panels / native docking rather than as palette
  commands.

Not ported (gaps):

- macOS NSServices right-click "Open in hxy": needs a real app bundle with an
  Info.plist NSServices declaration; gpui runs unbundled. Double-click /
  Apple-Events open-with IS covered via gpui-native `on_open_urls`.
- New / scratch buffer (`Untitled N`) and Paste / Paste as hex: no anonymous
  in-memory source and no clipboard splice-at-caret path in the gpui editor yet.
- `Memory` byte-cache debug panel: nothing constructs a `ByteCache` to inspect.
- Palette `Recent` / `QuickOpen` recents modes, `SetVirtualBase`, and an
  explicit reload command: recents live on the Welcome panel; virtual-base
  labeling and reload have no palette surface yet.

The wasm/web target and pixel-faithful cloning remain explicit non-goals (see
Goal). The `wat` crate resolving to different versions across the two lockfiles
is a build-infra item tracked at the milestone gate, not a feature deviation.
