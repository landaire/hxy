# M5 companion: gpui-component 0.5.1 -> 0.5.2 dock API translation table

Reference for the M5 dock-fork migration. Old = crates.io 0.5.1; New =
local fork `~/src/gpui-component` (0.5.2). Import path
`use gpui_component::dock::*;` is unchanged.

## Core model change
Layout is now immutable DATA (`PaneTree` of `PaneNode`), not a tree of
live `Entity` views. No `DockItem`; no `Panel/Tabs/Split/Tiles` view
entities in the tree. `DockArea` owns one `PaneTree` per region and
reconciles it into `TabGroup`/tiles/resizable entities. Editing = build
a `DockLayout` (data) and install it, or call an edit method taking
`PanelId`/`NodeId`.

Gotcha: base `Panel`/`PanelView` re-export as `BasePanel`/`BasePanelView`
(crates/ui/src/dock/mod.rs:35-37). The names `Panel`/`PanelView` in
`gpui_component::dock` are the PRESENTATION super-traits
(crates/ui/src/dock/panel.rs:71,144). App panels now impl BOTH.

## DockArea ctor / appearance
- `DockArea::new(id, version, window, cx)` same sig (dock_area.rs:149).
- Appearance separated: `.with_renderer(Rc<dyn DockAreaRenderer>)`
  (dock_area.rs:178). Skin: `DockSkin::dock_area(id, version, w, cx) ->
  (Entity<DockArea>, Rc<DockSkin>)` (crates/ui/src/dock/mod.rs:141), or
  `DockArea::new(..).with_renderer(DockSkin::new(cx))`. `.panel_style()`
  -> `DockSkin::set_panel_style` (mod.rs:181).

## Seed center/side: DockItem::* -> DockLayout builder (layout/builder.rs)
`h_split()`:44 `v_split()`:49 `tabs()`:54 `tiles()`:65; `.child(layout,
Option<Pixels>)`:82; `.panel(Entity<P>)`:94 `.panel_view(Arc<dyn
PanelView>, cx)`:112; `.tile*`:124/138; `.active_index(usize)`:156.
No window/cx in the builder. One-tab center:
`area.set_center(DockLayout::tabs().panel(editor.clone()), window, cx);`
No bare root panel -- lone panel is `tabs().panel(p)`.

## add / remove / docks
- `add_panel<P:Panel>(Entity<P>, DockPlacement, size: Option<Pixels>, w,
  cx)` dock_area.rs:391; Arc form `add_panel_view(Arc<dyn PanelView>,
  DockPlacement, Option<Pixels>, w, cx)`:416. (4th arg is now dock SIZE
  `Option<Pixels>`, not `Option<Bounds>`.) Tiles: `add_tile*`:438/459.
- `remove_panel<P:Panel>(Entity<P>, w, cx)` :573 -- NO placement arg,
  searches all regions. `remove_panel_from_all_docks` no longer needed.
- `set_dock(DockPlacement, DockLayout, w, cx)` :270 replaces
  `set_left/right/bottom_dock`; NO size/open args. `set_center(layout,
  w, cx)`:256 (or `set_dock(Center,..)`:277). `remove_dock(placement,
  w, cx)`:295. `set_dock_size(placement, Pixels, w, cx)`:372,
  `dock_size(placement)->Option<Pixels>`:368. `toggle_dock`:325 same.
  `is_dock_open(placement)->bool`:317 (no cx). `has_dock`:310 same.
  `is_dock_collapsible`:348 / `set_dock_collapsible`:354.
- `DockPlacement` still `Center/Left/Right/Bottom` (state.rs:191).

## Drag-split: TabPanel::add_panel_at -> DockArea edits (take NodeId/PanelId)
- `split_at(node: NodeId, panel: PanelId, placement: Placement, w, cx)`
  :655 (new group beside node). `Placement` = `gpui_component::Placement`
  {Left,Right,Top,Bottom}.
- `move_panel(panel: PanelId, target: InsertTarget, w, cx)` :584.
- `InsertTarget` (layout/edit.rs:12):
  `Tabs{node:NodeId, ix:Option<usize>, activate:bool}` |
  `Split{node:NodeId, placement:Placement, size:Option<Pixels>}` |
  `Tile{node:NodeId, bounds:Bounds<Pixels>}`.
- `PanelId::from(entity.entity_id())` (node.rs:33) or `panel.panel_id(cx)`.

## Tree walking: DockItem/.items() -> PaneTree/PaneNode/PaneRef (biggest)
- `DockArea::layout(placement) -> Option<&PaneTree>` :213 (Center always
  Some; NO whole-area single tree).
- `PaneTree`: `root()->&PaneNode`:64, `find_node(NodeId)`:99,
  `find_panel_node(PanelId)->Option<NodeId>`:104, `node_ids()`:82,
  `panels()->impl Iterator<PanelId>`:89.
- `PaneNode`: `id()->NodeId`:126, `kind()->PaneRef`:130,
  `walk(&mut impl FnMut(&PaneNode))` pre-order DFS:158.
- `PaneRef<'a>` (node.rs:100) -- NO Panel leaf variant (panels live in
  Tabs/Tiles):
  `Split{axis:Axis, children:&[PaneNode], sizes:&[Option<Pixels>]}` |
  `Tabs{panels:&[PanelId], active_ix:usize}` |
  `Tiles{panels:&[TilePanel]}` (TilePanel::{panel()->PanelId, bounds(),
  z_index()}). `NodeKind` owned enum is private -- match `PaneRef` via
  `.kind()`.
- Resolve PanelId -> view: `DockArea::panel(PanelId) -> Option<&Arc<dyn
  PanelView>>` :221. `DockArea::is_empty(placement, cx)`:235.
- Live `TabGroup` (if held): `panels()->&[Arc<dyn PanelView>]`
  (tab_group.rs:191), `active_ix()`:195, `active_panel(cx)`:202,
  `node()`:187.
- Enumerate all leaves across regions:
  `for placement in [Center,Left,Right,Bottom] { let Some(t) =
  area.layout(placement) else {continue}; t.root().walk(&mut |n| match
  n.kind() { PaneRef::Tabs{panels,active_ix}=>..., PaneRef::Tiles{..}=>...,
  PaneRef::Split{..}=>{} }); }`

## PanelView methods
- base (BasePanelView, panel.rs): `focus_handle(cx)`:109,
  `panel_id(cx)->PanelId`:100 (WAS EntityId), `view()`:108,
  `closable(cx)`:101, `dump(cx)->PanelState`:110, `zoomable(cx)->bool`:102.
- skin (crates/ui/src/dock/panel.rs): `tab_name(cx)->Option<SharedString>`
  :145, `title(w,cx)->AnyElement`:146, `title_style`/`title_suffix`/
  `toolbar_buttons`/`dropdown_menu`/`inner_padding`:147-152,
  `zoom_control(cx)->Option<PanelControl>`:151.
- Recover skin handle from base Arc: `PanelHandle::of(&Arc<dyn
  BasePanelView>) -> Option<&PanelHandle>` (crates/ui/src/dock/panel.rs:219);
  wrap a panel `panel_handle(entity)`:313.

## Panel trait (impl by app panels) -- now TWO traits
- base `Panel` (panel.rs:21, `EventEmitter<PanelEvent>+Render+Focusable`):
  REQUIRED `panel_name(&self)->&'static str`:23. Optional: `visible`:28,
  `closable`:34, `zoomable(&self,cx)->bool`:40, `set_active`:53,
  `set_zoomed`:61, `on_added_to(WeakEntity<TabGroup>,..)`:68 (WAS
  TabPanel), `on_removed`:82, `dump(cx)->PanelState`:90.
- skin `Panel` (crates/ui/src/dock/panel.rs:71, all optional): `tab_name`
  :76, `title`:82, `title_style`:87, `title_suffix`:92, `toolbar_buttons`
  :101, `dropdown_menu`:110, `zoom_control`:131, `inner_padding`:137.

## register_panel / persistence / events
- `register_panel(cx, panel_name, Fn(PanelBuildContext, &mut Window, &mut
  App) -> Arc<dyn PanelView>)` registry.rs:111. `PanelBuildContext`:
  `.dock_area()->WeakEntity<DockArea>`, `.state()->&PanelState`,
  `.info()->&PanelInfo`. Skin panel: return
  `Arc::new(PanelHandle::new(entity))`.
- `DockArea::dump(cx)->DockAreaState`:889 same. `load(state, w, cx) ->
  anyhow::Result<()>`:810 (NOW returns Result). `DockAreaState`/
  `PanelState`/`PanelInfo` names + fields unchanged (state.rs).
- `DockEvent` (dock_area.rs:39): `DragDrop{item:AnyDrag, target:DropTarget}`
  (WAS `DragDrop(AnyDrag)`); `LayoutChanged` unchanged. New
  `TabGroupEvent` (tab_group.rs:30) is internal container->area; the
  DockArea subscribes itself (dock_area.rs:1204) -- drive app logic off
  `DockEvent::LayoutChanged` + `DockArea::layout()`. `PanelEvent`
  {ZoomIn,ZoomOut,LayoutChanged} unchanged. No `StackPanelEvent`.

## No direct equivalent (closest path)
- `DockItem::view()` per-node Entity -> nodes are data; use
  `DockArea::panel(PanelId)`; hold `Entity<TabGroup>` only if base hands
  it (renderer / `on_added_to`).
- `TabPanel`/`StackPanel`/`Tiles` as app-constructed views -> gone; use
  `DockLayout`. `TabPanel` behavior now = base `TabGroup`.
- `set_zoomed_in::<P>(Entity<P>,..)` -> `set_zoomed_in(node: NodeId, w,
  cx)`:702 (names a CONTAINER node, not a panel); `set_zoomed_out`:711.
- `DockItem::find_panel(Arc)` -> `PaneTree::find_panel_node(PanelId)`
  (tree.rs:104).
