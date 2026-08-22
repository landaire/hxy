# M5: migrate hxy-gpui onto the gpui-component 0.5.2 fork (dock rewrite)

Foundational milestone unlocking full egui_dock parity. The local fork
`~/src/gpui-component` (jj-colocated) is a ground-up rewrite of the dock:
the layout engine moved into a new `gpui-base` crate and became pure
data (a `PaneTree` of split/tabs/tiles nodes addressed by stable
`NodeId`/`PanelId`, reconciled into entities after each edit). Every
region -- center AND all side docks -- now nests arbitrarily. That
rewrite is the path to egui_dock parity (per the M5 investigation).

This milestone is the migration itself: adopt the fork and port the
app's dock usage. The parity features it unlocks (close button, bottom
dock, palette split/merge/move verbs, nested-workspace guard, tab
switcher, tear-off windows) are separate follow-on work.

## Dependency wiring (done)

`gpui/Cargo.toml`:
- `gpui-component` -> path dep `../../../src/gpui-component/crates/ui`
  (the fork's 0.5.2 is unpublished; path dep during fork development,
  swap to the pushed git fork once it lands).
- `gpui` -> zed git rev `e0931d5a9dbf4f781b336fdf448739e74a2ac0b5`
  (the fork resolves `gpui` from zed git and depends on git-only zed
  crates like `gpui_platform`/`gpui_macros` that never ship to
  crates.io; the app MUST resolve the same git `gpui` or the two `gpui`
  instances are distinct crates and won't interoperate).
- The three dev-dep `gpui = { version = "=0.2.2", ... test-support }`
  pins -> `{ workspace = true, features = ["test-support"] }`.

Gotcha (resolved): at this zed rev `runtime_shaders` is a feature of
`gpui_platform`, not `gpui`. Enabled via a direct `gpui_platform`
workspace dep with `features = ["runtime_shaders"]` that hxy-gpui pulls
in; Cargo feature unification turns it on for the single `gpui_platform`
the graph shares. Without it the macOS build needs full Xcode for
`xcrun metal`.

This zed rev pulls a different render stack (wgpu/naga/cosmic-text), so
the first build compiles the whole gpui stack from source (slow).

## API port scope

Old 0.5.1 dock API -> new 0.5.2 fork API. Concentrated in
`workspace.rs`, plus `gpui_dock_picker/src/lib.rs`, `panels/file.rs`,
`panels/strings.rs`, `panels/workspace_host.rs`, `panels/inspector.rs`,
`panels/mod.rs`, `persist.rs`. Usage magnitude (hxy-gpui + picker):
DockItem 181, .items() 108, DockArea 92, PanelView 80, Placement 69,
DockPlacement 65, TabPanel 47, add_panel 38, remove_panel 32,
PanelEvent 30, register_panel 16, PanelRegistry 13.

Key shape changes:
- Center/side seeded via `DockLayout` builder (`h_split`/`v_split`/
  `tabs`/`panel`/`child`) + `set_center`/`set_dock`, not `DockItem::*`.
- `DockItem` tree + `.items()` walk -> new pure-data node tree
  (`PaneTree`/`PaneNode`), walked via the node API; enumerate leaves +
  active index from that. This is the bulk of the port.
- `add_panel(placement)` / `set_*_dock` / `add_panel_at` / drag-split
  -> `add_panel` / `set_dock` / `move_panel(InsertTarget)` / `split_at`.
- `PanelView`/`Panel` traits, `PanelRegistry`, dump/load, and event
  enums (`DockEvent`/`TabGroupEvent`/`PanelEvent`) all shift; see the
  translation table produced during the port.

## Staging / progress

1. Wire deps -- DONE. Single zed rev (`e0931d5`) after pinning the fork's
   own zed git deps; `runtime_shaders` via `gpui_platform`.
2. `hxy-view-gpui` -- DONE (gpui `focus(_, cx)` + `ShapedLine::paint`
   gained `TextAlign`/align-width args).
3. `gpui_dock_picker` -- DONE (rewrote enumeration onto
   `layout()`/`PaneTree`/`PaneRef`; `PanelHandle::of` for `tab_name`).
4. `hxy-gpui`:
   - Registration (`panels/mod.rs`), `console_view`, `menu.rs`
     (`Menu.disabled`), `main.rs` (`gpui_platform::application()`),
     `palette` -- DONE.
   - All 14 panels -- DONE (base/skin `Panel` split; plus incidental
     0.5.2 deltas the port surfaced: `PanelState::new(name)`,
     `Table`->`DataTable`, `TableDelegate::column` returns owned,
     chart `Bar` `.cross/.base/.value`, `InputEvent::PressEnter{shift}`,
     `Dialog::confirm()` removed -> `DialogButtonProps::show_cancel`,
     `PixelsExt::as_f32` now inherent, `track_scroll(&h)`).
   - `workspace.rs` (~100) + `panels/workspace_host.rs` (~18): the
     dock-orchestration core -- IN PROGRESS. Error count 204 -> 122
     before this step; only these two files remained.
5. Verify -- DONE. `hxy-gpui` compiles + links (binary builds);
   `cargo test -p hxy-gpui` = 311 passed / 0 failed. Committed
   (hxy `npkqlq`; fork: rev-pins + window_handle guards +
   `pub remove_panel_id`). Remaining confidence step: launch the real
   app (blocked here by the single-instance socket of the user's running
   pre-change build; quit it and relaunch to eyeball tabs/docking).

   Fixes made to reach 0 failures (all root-caused):
   - Fixed: gpui `TestWindow::window_handle` panics under the fork's
     `Root::new` (via `install_window_hit_test_forwarder`). The forwarder
     now probes `ns_view` under `catch_unwind` (fork
     `crates/base/src/dock/../macos_accessibility.rs`) so headless/test
     windows no-op instead of panicking (fixed ~102 tests).
   - Fixed: the 0.5.2 dock does not focus a freshly added panel (0.5.1's
     TabPanel did), so opening a file left focus on the workspace root and
     keystrokes never reached the pane. `add_file_panel` now focuses the
     added panel (delegates to the `HexPane`).
   - Test windows panicked in `Root::new` + `Input::render` (gpui
     `TestWindow::window_handle` is `unimplemented!`, not `Err`): fork
     `catch_unwind` guards in `macos_accessibility.rs` +
     `input/base/native.rs` (~107 tests).
   - The 0.5.2 dock does not auto-focus a freshly added panel: focus the
     panel in `add_file_panel` + `open_strings_for_active_file` (keystrokes
     reach the pane / the reference-file fallback).
   - Bare `add_panel(Entity)` drops the skin tab title (tab bar falls back
     to `panel_name`): wrap all center adds + the inspector/welcome seeds
     in `add_panel_view(Arc::new(PanelHandle::new(..)))` (real file names
     in tabs; the pane picker labels leaves correctly).
   - Test `set_value` is silent in 0.5.2 (suppresses events): the palette /
     search-bar test helpers emit `InputEvent::Change` so state syncs.
   - `cmd-w` on an unrecognized/InvalidPanel tab: use the fork's new
     `pub remove_panel_id` instead of a warn no-op.
   Still worth a manual eyeball (not test-covered): chart `Bar` axes
   (visualizer), workspace_host type-gated eject (could now use
   `remove_panel_id`), inspector-dock double `LayoutChanged`.

## Follow-on (separate milestones, ride on this)

- Per-tab close (X) button (fork skin patch: `Tab::suffix` + close btn).
- Console in a real bottom dock + true toggle.
- Palette Split/Merge/MoveTab verbs; nested side splits (free on 0.5.2).
- Nested-workspace cross-area drag ownership guard (fork `move_panel`).
- Tab fuzzy quick-switcher.
- Tear-off / floating windows (multi-surface; XL, in scope per owner).
