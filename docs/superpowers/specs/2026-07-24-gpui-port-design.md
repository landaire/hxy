# GPUI port of hxy: design

Date: 2026-07-24
Status: approved (pending spec review)

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

- gpui and gpui_platform pinned to the zed-industries/zed rev that
  gpui-component pins; gpui-component pinned to a specific rev. All pins in
  the workspace Cargo.toml. Both are pre-1.0; pinning insulates against API
  churn. crates.io releases exist but lag; the git route is what upstream
  docs recommend.
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
- M3: strings, entropy, checksums, compare/diff, visualizers, VFS browser,
  file watching, snapshots, save.
- M4: template runner (010/ImHex), plugins and mounts, IPC single-instance,
  settings persistence, i18n sweep, welcome and console tabs.

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
