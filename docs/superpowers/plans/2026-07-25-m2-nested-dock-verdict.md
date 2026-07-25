# Nested-dock (workspace-in-a-tab) spike verdict

Task 8 of the M2 GPUI workbench plan. Investigates whether a
`gpui_component::dock::Panel` can wrap and own an inner `DockArea` --
needed for M3+'s `Tab::Workspace` (a tab whose body is itself a full
`egui_dock`-style workspace, used by VFS mounts in the egui app).

Spike code: `gpui/hxy-gpui/src/panels/workspace_spike.rs`, gated behind the
`dock-spike` Cargo feature on `hxy-gpui` (off by default; not wired into
`main.rs`). Run with `cargo test -p hxy-gpui --features dock-spike`.

Source citations below are against the vendored crates.io package at
`~/.local/share/cargo/registry/src/index.crates.io-1949cf8c6b5b557f/gpui-component-0.5.1/src/dock/`
(`GC` = that directory), matching
`docs/superpowers/plans/2026-07-25-m2-gpui-component-notes.md`.

## Headline

**Rendering and persistence work mechanically with zero upstream changes;
cross-area drag-and-drop is unguarded and would silently move panels
between the inner and outer dock. Three M3+ options were considered
(wrapper with guards, flat-dock emulation of workspaces, an upstream
patch to `tab_panel.rs`); recommended: wrapper with guards -- see
"Recommended M3+ approach" below.**

## What was built

`WorkspaceHostPanel` (`workspace_spike.rs`): a `Panel` whose `render`
returns its own `Entity<DockArea>` as a child element. Its inner dock area
is seeded with two dummy leaf panels (`DummyPanel` A/B, trivial
`Panel + Render + Focusable` structs). Three names registered with
`PanelRegistry`: `SpikeWorkspaceHostPanel`, `SpikeDummyPanelA`,
`SpikeDummyPanelB`.

## (a) Rendering and focus inside a tab

**Result: works, no upstream changes needed.**

`DockArea` is a plain `Render + EventEmitter<DockEvent>` entity (GC
`mod.rs:1085-1086`); nothing stops a `Panel` from holding an
`Entity<DockArea>` and childing it in `render`. Test
`inner_dock_area_renders_and_focuses_through_wrapper` builds a
`WorkspaceHostPanel` as a window's root view, then:

1. Reads the inner dock area's live `items()` tree and confirms both dummy
   panels are present as real leaves (not placeholders).
2. Takes the `FocusHandle` of the inner `SpikeDummyPanelB` leaf --
   obtained through `PanelView::focus_handle`, i.e. the same object-safe
   path the outer dock area itself uses to focus tabs -- and moves window
   focus to it directly (`window.focus`).
3. Confirms `window.focused()` now equals that handle.

gpui focus is a flat, per-window tree keyed by `FocusHandle`, not scoped by
visual nesting: a click or programmatic focus on a panel living inside the
inner `DockArea` lands exactly where it would if that `DockArea` were the
window's only dock area. The wrapper's own `FocusHandle` (required by
`Panel: Focusable`) is a separate handle reachable only via its own root
div's `track_focus`; it does not intercept or redirect focus aimed at
inner-dock descendants.

## (b) Dump/load round-trip through PanelInfo::Panel json

**Result: works, but persistence does not compose automatically -- it is
fully hand-composed, exactly as the upstream notes predicted.**

`DockArea::dump` (`mod.rs:927`) walks the live panel tree calling each
leaf's `Panel::dump` (default: `PanelState::new(self)`, i.e. just the
panel's name, GC `panel.rs:156-158`). It has no idea `WorkspaceHostPanel`
owns a second `DockArea` underneath it, so without an override the inner
layout would be silently dropped on every save.

`WorkspaceHostPanel::dump` overrides this: it calls `self.dock.read(cx).dump(cx)`
on its OWN inner dock area, serializes the resulting `DockAreaState` to a
`serde_json::Value`, and stashes it under an `"inner_dock_state"` key inside
`PanelInfo::panel(json!({ ... }))`. `WorkspaceHostPanel::restore` (the
`PanelRegistry` deserialize closure) does the inverse: build a fresh inner
`DockArea`, pull `"inner_dock_state"` back out of the `PanelInfo::Panel`
payload, `serde_json::from_value` it back to a `DockAreaState`, and call
`DockArea::load` on the inner dock area -- which in turn resolves
`SpikeDummyPanelA`/`B` through the SAME `PanelRegistry`, one level down.

Test `dump_load_round_trips_inner_dock_state_through_registry` exercises
the full realistic path: build an OUTER dock area with the host panel as
its one center tab, `dump()` it, round-trip the result through actual JSON
text (`serde_json::to_string` / `from_str`, not just an in-memory struct
clone -- this is the on-disk persisted-layout-file path), `load()` a FRESH
outer dock area from that JSON, and confirm:

- The outer `PanelRegistry` rebuilt a real `WorkspaceHostPanel` (not
  `InvalidPanel`).
- Its inner dock area, reached by reading straight through the rebuilt
  entity, has both dummy panels back, in the original order.

This confirms `DockAreaState`/`PanelState`/`PanelInfo` all round-trip
through `serde_json` cleanly at two nesting levels with no library changes.
The cost is that the wrapper panel owns 100% of the serialization glue:
every field of nesting-specific state (which inner panels exist, their
order, split geometry, active tab) has to be threaded through the
wrapper's `dump`/`restore` by hand, and any future gpui-component upgrade
that changes `DockAreaState`'s shape silently changes the wrapper's
persisted json format too (it is not a documented/stable serialization
contract, just "whatever `Serialize` derives today").

## (c) Cross-area drag failure mode

**Not driven empirically** -- gpui 0.2.2's `test-support` exposes
`simulate_keystrokes` and direct `window.focus`/entity mutation, but no
built-in helper to synthesize a full mouse-drag-and-drop sequence
(press-move-release with the intermediate `DragPanel` payload gpui-component
builds internally via `.on_drag`) within this task's timebox. What follows
is **source-analysis, not an empirical test result**, against
GC `tab_panel.rs`.

### The mechanism

- `DragPanel` (`tab_panel.rs:36-39`) is the drag payload: `{ panel:
  Arc<dyn PanelView>, tab_panel: Entity<TabPanel> }` -- it carries the
  SOURCE `TabPanel` entity, not the source `DockArea`. The struct is
  `pub(crate)`, so no app code outside gpui-component can construct or
  intercept one.
- Every `TabPanel` privately holds `dock_area: WeakEntity<DockArea>`
  (`tab_panel.rs:69`), set once at `TabPanel::new` (`tab_panel.rs:164`,
  per the notes doc) and never compared against anything.
- `on_panel_drag_move` (`tab_panel.rs:896-919`) computes a split-edge
  hover state (`will_split_placement`) purely from cursor position within
  the target `TabPanel`'s bounds -- it never looks at `drag.tab_panel` or
  any dock-area identity at all.
- `on_drop` (`tab_panel.rs:924-972`) is the actual mutation:
  1. `is_same_tab = drag.tab_panel == cx.entity()` -- compares the SOURCE
     `TabPanel` entity to `self` (the DROP-TARGET `TabPanel`). This is
     the only identity check in the whole function.
  2. If not the same tab panel, it calls `drag.tab_panel.update(cx, |view,
     cx| { view.detach_panel(...); view.remove_self_if_empty(...) })` --
     unconditionally detaching the panel from whatever `TabPanel` it came
     from, regardless of which `DockArea` that `TabPanel` belongs to.
  3. It then inserts the panel into `self` (the target `TabPanel`), either
     via `split_panel` (`tab_panel.rs:975`, creating a new `TabPanel` and
     re-splitting the target's parent `StackPanel`) or as a plain tab.
  4. It emits `PanelEvent::LayoutChanged` and returns.

At no point does `on_drop` compare `self.dock_area` (the target's) against
`drag.tab_panel`'s `dock_area` (the source's) -- both fields exist, both
are in scope for the same module, and neither is read in this path. This
matches and confirms the notes doc's claim verbatim
("No dock-area identity check in on_drop (tab_panel.rs:924-957)").

### Predicted failure mode

If two `DockArea`s are ever both live in the same window (exactly the
`WorkspaceHostPanel` shape: an outer `DockArea` and an inner one, both
mounted, both with visible `TabPanel`s) and a user drags a tab from one
into the other:

- The drag payload's underlying gpui `on_drag`/`on_drop` wiring is
  per-`TabPanel`, not per-`DockArea`, and nothing during the drag restricts
  a drop target to `TabPanel`s sharing the source's `dock_area`. A drop
  handler on an inner-dock `TabPanel` will happily accept a `DragPanel`
  whose source `TabPanel` belongs to the OUTER `DockArea`, and vice versa.
- The panel is mechanically moved: `detach_panel` removes it from the
  source `TabPanel`'s `panels: Vec<Arc<dyn PanelView>>`
  (`tab_panel.rs:338`), and it is re-inserted into the target's. Both
  `TabPanel`s still hold their OWN `dock_area: WeakEntity<DockArea>`
  unchanged -- so the panel now lives inside a `TabPanel` whose
  `dock_area` points at a different `DockArea` than the one that "owns"
  the panel by whatever the app's book-keeping assumes.
  `WorkspaceHostPanel` in particular tracks nothing about which panels are
  "supposed" to belong to its inner dock vs. the outer one -- it only
  round-trips whatever `self.dock.read(cx).dump(cx)` reports at save time,
  so this would look like a perfectly normal, successful move right up
  until the next persistence round-trip or any app logic that assumed a
  panel's identity implies a fixed dock area (e.g. this app's own
  `open_files`-by-path reuse registry in `workspace.rs`, which is exactly
  this class of assumption).
- `remove_self_if_empty` (`tab_panel.rs:353`) would still fire correctly
  on the source side (it only checks `self.panels.is_empty()`), so an
  inner dock area drained down to zero tabs via drags-out would correctly
  self-collapse -- the corruption is specifically "a panel ends up parented
  under the wrong `DockArea`'s tree", not a crash or a leaked empty
  `TabPanel`.
- No tear-off-to-new-window support exists at 0.5.1 either (per the notes
  doc), so this is strictly an in-window inner/outer leak, not a
  cross-window one.

This is a plausible, code-path-confirmed failure mode, not a
speculative one: every step above traces through an actual function in
`tab_panel.rs` with no branch that would reject or redirect a
cross-dock-area drag. It has not been reproduced by driving a real pointer
event in a test.

## Recommended M3+ approach

Three options were considered, per the task brief:

1. **Wrapper with guards (recommended for M3, if workspace tabs ship).**
   Keep the `WorkspaceHostPanel` shape this spike validates for (a) and
   (b) -- it costs nothing upstream and both exercises passed cleanly.
   Close the (c) gap defensively at the APP layer rather than patching
   gpui-component: give every `TabPanel`-hosting `DockArea` in the window
   a distinct identity the app already controls (the `id: impl
   Into<SharedString>` passed to `DockArea::new`), and reject/undo drags
   that cross that boundary by observing `DockEvent::LayoutChanged` after
   the fact and diffing dumped panel sets against expected membership
   (comparable in shape to this app's existing `resync_center_if_stale`/
   `center_cache_missing_a_live_leaf` staleness-healing pattern in
   `workspace.rs`, which already reconciles the live tree against a cache
   after the fact). This is app-level, ships no vendored/forked
   gpui-component code, and matches this codebase's existing style of
   working around 0.5.1 dock quirks by post-hoc reconciliation rather than
   patching the dependency. Downside: it is reactive (the drag DOES
   complete for one frame before any correction), so a user could
   theoretically observe a flicker, and any app state that reacts
   synchronously to `LayoutChanged` before the guard runs sees the
   momentarily-wrong tree.

2. **Flat-dock emulation of workspaces (no nesting at all).** Represent
   each VFS-mount "workspace" as a distinguishable GROUP of ordinary
   panels/tabs in the SAME single top-level `DockArea`, tagged by a
   workspace id in each panel's own state, rather than as a genuinely
   separate nested dock. Sidesteps (c) entirely (only one `DockArea`
   exists, so there is no cross-area boundary to violate) and avoids the
   hand-composed persistence cost of (b). Downside: loses real UI
   separation (a workspace's own left/right/bottom docks, its own
   center-split layout independent of the outer one) that `Tab::Workspace`
   provides in the egui app; likely requires visible compromises in
   workspace-switching UX or a custom "logical grouping" layer built on
   top of flat tabs.

3. **Upstream patch.** The fix for (c) is small and precisely locatable:
   `on_drop` (`tab_panel.rs:924`) already has both `self.dock_area` and
   `drag.tab_panel`'s `dock_area` in scope (both `pub(crate)`-reachable
   within the same module) and could early-return (or otherwise refuse the
   drop) when `self.dock_area.entity_id() != drag.tab_panel.read(cx).dock_area.entity_id()`.
   This is a single well-contained module change, not a redesign. Risk:
   0.5.1 is pulled from crates.io (`gpui-component = "0.5.1"` pinned per
   the M2 plan), so this requires either forking/vendoring the crate or
   upstreaming and waiting on a release -- both add ongoing maintenance
   burden disproportionate to a capability (nested workspaces) that only
   VFS mounts need, arriving M3+.

**Recommendation:** go with (1), wrapper-with-guards, if/when M3 actually
needs `Tab::Workspace`. It reuses the exact mechanism this spike validated
end-to-end for (a)/(b), needs no dependency fork, and fits the app's
established pattern of defending against 0.5.1's rough edges (see
`workspace.rs`'s existing stale-cache healing) rather than either
patching the vendored crate or giving up real nested-workspace UX. Revisit
option 3 only if the guard's reactive-correction flicker proves visible or
troublesome in practice, at which point a small upstream/fork patch to
`on_drop` becomes proportionate.
