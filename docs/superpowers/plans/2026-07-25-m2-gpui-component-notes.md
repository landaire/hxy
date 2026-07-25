# gpui-component 0.5.1 API notes (verified against vendored crates.io source)

Source roots (all paths below relative to these unless absolute):
- GC  = /Users/lander/.local/share/cargo/registry/src/index.crates.io-1949cf8c6b5b557f/gpui-component-0.5.1
- GP  = /Users/lander/.local/share/cargo/registry/src/index.crates.io-1949cf8c6b5b557f/gpui-0.2.2

Package contents: only `src/`, `tests/fixtures/layout.json`, `locales/`,
Cargo.toml, README. NO `examples/` directory ships in the crates.io package
(verified by ls of GC root). The repo's examples/story crates are git-only.
Closest reference for a docked multi-panel app is the fixture
GC/tests/fixtures/layout.json plus the state round-trip test at
GC/src/dock/state.rs:240-282 (uses "StoryContainer" panels from the git-only
story example).

Crate init: `gpui_component::init(cx)` must be called at app entry
(GC/src/lib.rs:97-116). It inits theme, root, dock (PanelRegistry), input,
list, dialog, popover, menu, select, table, text, tree.

## 1. Dock module (GC/src/dock/)

Files: mod.rs (DockArea/DockItem), dock.rs (Dock/DockPlacement), panel.rs
(Panel/PanelView/PanelRegistry), stack_panel.rs, tab_panel.rs, tiles.rs,
state.rs, invalid_panel.rs.

### DockArea (GC/src/dock/mod.rs:43-73, 513-1084)
- `DockArea::new(id: impl Into<SharedString>, version: Option<usize>, window, cx)` mod.rs:514
- `set_center(item: DockItem, window, cx)` mod.rs:596 (`set_root` is deprecated alias, mod.rs:588-591)
- `set_left_dock(panel: DockItem, size: Option<Pixels>, open: bool, window, cx)` mod.rs:603
- `set_bottom_dock(...)` mod.rs:625, `set_right_dock(...)` mod.rs:647 (same shape)
- `set_locked(bool, ...)` mod.rs:670, `is_locked()` mod.rs:676
- `has_dock(DockPlacement)` mod.rs:681, `is_dock_open(DockPlacement, cx)` mod.rs:691
- `set_dock_collapsible(Edges<bool>, window, cx)` mod.rs:715
- `toggle_dock(DockPlacement, window, cx)` mod.rs:763
- `set_toggle_button_visible(bool, cx)` mod.rs:784
- `add_panel(Arc<dyn PanelView>, DockPlacement, bounds: Option<Bounds<Pixels>>, window, cx)` mod.rs:789
  (Center placement adds into first Tabs found; side placements create the dock if absent)
- `remove_panel(Arc<dyn PanelView>, DockPlacement, window, cx)` mod.rs:846
- `remove_panel_from_all_docks(...)` mod.rs:883
- `load(state: DockAreaState, window, cx) -> anyhow::Result<()>` mod.rs:898
- `dump(cx: &App) -> DockAreaState` mod.rs:927
- `panel_style(PanelStyle)` builder mod.rs:576; `set_version` mod.rs:582; `id()` mod.rs:1036
- zoom: `set_zoomed_in::<P: Panel>(Entity<P>, ...)` mod.rs:1040, `set_zoomed_out` mod.rs:1050
- `impl EventEmitter<DockEvent> for DockArea` mod.rs:1085

### DockEvent (GC/src/dock/mod.rs:31-40)
Only two variants at 0.5.1:
- `LayoutChanged` (doc: emitted on every layout change, debounce before saving)
- `DragDrop(AnyDrag)` (emitted from Tiles item drop, mod.rs:562-573)

### DockItem (GC/src/dock/mod.rs:77-109)
Variants: `Split { axis, size, items, sizes, view: Entity<StackPanel> }`,
`Tabs { size, items: Vec<Arc<dyn PanelView>>, active_ix, view: Entity<TabPanel> }`,
`Panel { size, view: Arc<dyn PanelView> }`,
`Tiles { size, items: Vec<TileItem>, view: Entity<Tiles> }`.
Factories:
- `DockItem::split(axis, items, &WeakEntity<DockArea>, window, cx)` mod.rs:175
- `v_split` mod.rs:187 / `h_split` mod.rs:197
- `split_with_sizes(axis, items, sizes: Vec<Option<Pixels>>, ...)` mod.rs:210
  WARNING: 0.5.1 has a duplicated add loop (mod.rs:221-231 adds each panel
  twice); harmless only because StackPanel::insert_panel dedupes via
  index_of_panel (stack_panel.rs:218-221).
- `DockItem::tabs(Vec<Arc<dyn PanelView>>, ...)` mod.rs:320; `tab::<P: Panel>(Entity<P>, ...)` mod.rs:333
- `DockItem::panel(Arc<dyn PanelView>)` mod.rs:256
- `DockItem::tiles(items, metas: Vec<impl Into<TileMeta>>, ...)` mod.rs:266
- builders: `.size(Pixels)` mod.rs:147, `.active_index(usize)` (Tabs only) mod.rs:159
- `view() -> Arc<dyn PanelView>` mod.rs:368; `find_panel` mod.rs:378;
  `add_panel` mod.rs:396; `remove_panel` mod.rs:449; `set_collapsed` mod.rs:474

### Dock + DockPlacement (GC/src/dock/dock.rs)
- `DockPlacement { Left, Bottom, Right, Center }` dock.rs:29 (Serialize/Deserialize)
- `Dock::left/bottom/right(WeakEntity<DockArea>, window, cx)` dock.rs:113-137
- `set_panel(DockItem, ...)` dock.rs:232, `is_open` dock.rs:237, `toggle_open`
  dock.rs:241, `size`/`set_size` dock.rs:248-257, `set_open` dock.rs:259,
  `add_panel` dock.rs:269, `remove_panel` dock.rs:281,
  `set_collapsible` dock.rs:140, `from_state` (pub(super)) dock.rs:148

### Panel trait (GC/src/dock/panel.rs:54-164)
`pub trait Panel: EventEmitter<PanelEvent> + Render + Focusable`
Required method: only `fn panel_name(&self) -> &'static str` (panel.rs:59;
must be stable across versions, used for deserialization).
Defaulted methods:
- `tab_name(&self, cx) -> Option<SharedString>` panel.rs:64
- `title(&mut self, window, cx) -> impl IntoElement` panel.rs:69 (default "Unnamed" via i18n)
- `title_style(&self, cx) -> Option<TitleStyle>` panel.rs:74; `title_suffix` panel.rs:81
- `closable(&self, cx) -> bool` (default true) panel.rs:92
- `zoomable(&self, cx) -> Option<PanelControl>` (default Some(Menu)) panel.rs:99
- `visible(&self, cx) -> bool` panel.rs:106
- `set_active(&mut self, active: bool, window, cx)` panel.rs:115
- `set_zoomed(&mut self, zoomed: bool, window, cx)` panel.rs:122
- `on_added_to(&mut self, WeakEntity<TabPanel>, window, cx)` panel.rs:125; `on_removed` panel.rs:134
- `dropdown_menu(&mut self, PopupMenu, window, cx) -> PopupMenu` panel.rs:137 (per-panel menu)
- `toolbar_buttons(&mut self, window, cx) -> Option<Vec<Button>>` panel.rs:147
- `dump(&self, cx) -> PanelState` panel.rs:156 (default: PanelState::new(self),
  i.e. just panel_name; OVERRIDE to persist per-panel data into PanelInfo::Panel json)
- `inner_padding(&self, cx) -> bool` panel.rs:161
`PanelEvent { ZoomIn, ZoomOut, LayoutChanged }` panel.rs:11-15.
`PanelStyle { Auto, TabBar }` panel.rs:18-24. `PanelControl { Both, Menu, Toolbar }` panel.rs:33.

### PanelView (GC/src/dock/panel.rs:168-188)
Object-safe mirror trait, blanket `impl<T: Panel> PanelView for Entity<T>`
(panel.rs:190). So `Arc::new(entity)` is how concrete panels enter DockItem.

### PanelRegistry + load (GC/src/dock/panel.rs:293-371, state.rs)
- `register_panel(cx, panel_name: &str, deserialize_fn)` panel.rs:356 where fn is
  `Fn(WeakEntity<DockArea>, &PanelState, &PanelInfo, &mut Window, &mut App) -> Box<dyn PanelView>`.
- `PanelRegistry::build_panel(panel_name, ...)` panel.rs:332; unregistered names
  yield `InvalidPanel` placeholder (panel.rs:349, invalid_panel.rs).
- Restore path: `DockArea::load(DockAreaState)` -> `DockState::to_dock`
  (state.rs:46) and `PanelState::to_item` (state.rs:182-237): Stack ->
  split_with_sizes, Tabs -> DockItem::tabs + active_index, Panel ->
  registry build_panel wrapped in a single-tab Tabs, Tiles -> DockItem::tiles.

### TabPanel drag-drop (GC/src/dock/tab_panel.rs)
- `TabPanel::new(Option<Arc<dyn PanelView>>, WeakEntity<DockArea>, window, cx)` tab_panel.rs:162
- `add_panel` tab_panel.rs:239, `add_panel_at` tab_panel.rs:280, `remove_panel`
  tab_panel.rs:326, `active_panel` tab_panel.rs:193
- Drag payload `DragPanel { panel: Arc<dyn PanelView>, tab_panel: Entity<TabPanel> }`
  is pub(crate) (tab_panel.rs:36-39): apps cannot construct external drags.
- Tabs are `.on_drag(DragPanel...)` sources (tab_panel.rs:666, 755); drop
  targets on tab bar reorder (`on_drop` with Some(ix), tab_panel.rs:770-801)
  and on panel body split by edge: `on_panel_drag_move` sets
  `will_split_placement` Left/Right/Top/Bottom/center (tab_panel.rs:898-922);
  `on_drop` (tab_panel.rs:924-972) detaches from source TabPanel and either
  `split_panel` (tab_panel.rs:974, creates new TabPanel and inserts into parent
  StackPanel, re-splitting axis as needed) or inserts as tab. Emits
  PanelEvent::LayoutChanged. Empty source TabPanels self-remove
  (`remove_self_if_empty`).
- No dock-area identity check in on_drop (tab_panel.rs:924-957): drops across
  two DockAreas in one window would mechanically move the panel, but nothing in
  0.5.1 tests/uses that. No tear-off-to-new-window support.
- Toggle buttons for left/bottom/right docks render inside corner TabPanels
  (tab_panel.rs:615-617); `DockArea::set_toggle_button_visible` controls them.

### Can a panel contain another DockArea? (nesting verdict)
- DockArea does NOT implement Panel or Focusable; it is a plain
  Render + EventEmitter<DockEvent> entity (mod.rs:1085-1086). It cannot be put
  into a DockItem directly.
- A user Panel could own an `Entity<DockArea>` and render it as its body;
  nothing forbids it, but the crate ships zero examples/tests of nesting (no
  examples dir; tests/ contains only layout.json). Persistence would not
  compose automatically: outer dump() would record only the wrapper panel's
  PanelState; the wrapper must serialize the inner DockAreaState itself into
  its PanelInfo::Panel json and rebuild via its register_panel closure.
  Cross-area tab drags between inner/outer areas are unguarded (see above) and
  should be considered undefined behavior. Verdict: not supported out of the
  box; treat as DIY with real risk.

## 2. Modal / overlay system (GC/src/root.rs, dialog.rs, sheet.rs)

- Window-level layers managed by `Root` view, which MUST be the top-level view
  of the window (root.rs:210-213: "Must be the first view in the window").
  `Root::new(view, window, cx)` root.rs:240.
- Extension trait `WindowExt` on `Window` (root.rs:27-74) -- note it is named
  WindowExt at 0.5.1, not ContextModal:
  - `open_dialog(cx, |Dialog, window, cx| Dialog)` root.rs:45/118
  - `close_dialog` root.rs:144, `close_all_dialogs` root.rs:160,
    `has_active_dialog` root.rs:140
  - `open_sheet` / `open_sheet_at(Placement, ...)` root.rs:29-36 (side drawer)
  - `push_notification(impl Into<Notification>, cx)` root.rs:59/169
  - `remove_notification::<T>()` root.rs:178, `clear_notifications` root.rs:188
  - `focused_input(cx) -> Option<Entity<InputState>>` root.rs:205, `has_focused_input` root.rs:201
- IMPORTANT: Root::render (root.rs:~415) renders only the child view; the app's
  root view must itself render the layers each frame:
  `Root::render_dialog_layer(window, cx)` root.rs:335,
  `Root::render_sheet_layer` root.rs:305,
  `Root::render_notification_layer` root.rs:279 (each returns
  Option<impl IntoElement> to append last in the tree).
- Dialog builder (dialog.rs): `.title` :138, `.footer` :151, `.confirm()` :168,
  `.alert()` :177, `.button_props(DialogButtonProps)` :184, `.on_ok/.on_cancel/
  .on_close` :192-221, `.overlay(bool)` :255, `.overlay_closable` :263,
  `.keyboard(bool)` :269, `.w/.width/.max_w/.margin_top` :229-253.
- Focus handling: opening saves `previous_focus_handle` and focuses a fresh
  dialog handle (root.rs:122-137); closing focuses next dialog or restores
  previous focus (root.rs:144-158). Keybindings "escape" -> Cancel and
  "enter" -> Confirm bound in "Dialog" context (dialog.rs:24-25); dialog root
  div does `.track_focus(&self.focus_handle)` (dialog.rs:436). Dialogs stack
  (Vec<ActiveDialog>).

## 3. Command-palette building blocks

- No ready-made palette/picker component at 0.5.1, and no fuzzy matcher in the
  crate (grep: only aho-corasick literal search in input/search.rs and
  substring match in select.rs). App must bring its own fuzzy scorer (e.g.
  nucleo/fuzzy-matcher crate).
- Best base: `ListState<D: ListDelegate>` + `List` (GC/src/list/list.rs):
  - `ListState::new(delegate, window, cx)` list.rs:95; `.searchable(true)`
    list.rs:126 renders a query Input at top (an InputState created with
    i18n placeholder, list.rs:97) and calls
    `delegate.perform_search(&query, ...) -> Task<()>` on InputEvent::Change
    (list.rs:250-280, with loading spinner via set_loading).
  - `ListDelegate` (GC/src/list/delegate.rs:10-157): required
    `items_count(section, cx)` :30, `render_item(ix: IndexPath, ...) ->
    Option<Self::Item>` :37, `set_selected_index` :114; optional
    `perform_search` :15, `confirm(secondary, ...)` :125 (Enter/click),
    `cancel` :129 (Esc), sections/headers/footers, `render_empty` :69,
    `render_initial` :90, `loading`/`render_loading` :99-111,
    `load_more`/`load_more_threshold`/`is_eof` :131-156.
    Item type must impl `Selectable + IntoElement`.
  - Keys bound in "List" context: escape=Cancel, enter=Confirm,
    secondary-enter=Confirm{secondary}, up/down=SelectUp/Down (list.rs init fn).
  - `List::new(&Entity<ListState<D>>)` list.rs:681, `.search_placeholder(...)`
    list.rs:696, `.scrollbar_visible` list.rs:690. Virtualized (uses
    VirtualListScrollHandle; `scroll_to_item` list.rs:211).
- Palette recipe: WindowExt::open_dialog containing a List with
  searchable(true) (or render the Input + List manually), fuzzy-filter in
  perform_search, execute in confirm(). Select
  (GC/src/select.rs: SelectState/SelectDelegate/SearchableVec) is a
  dropdown-anchored variant; SearchableVec::perform_search is lowercase
  `contains` substring only (select.rs, impl around :364-379 region).
- rusthex: not part of this package; nothing to verify locally.

## 4. Input widgets (GC/src/input/)

- State/view split: `InputState` entity (state.rs:261) + `Input` element
  wrapper (input.rs:22) rendered as `Input::new(&state)`.
- `InputState::new(window, cx)` state.rs:343; builders: `.placeholder(...)`
  state.rs:467, `.multi_line(bool)` :420, `.auto_grow(min,max)` :426,
  `.code_editor(lang)` :452, `.searchable` :460, `.line_number` :473,
  `.rows(n)` :495, `.masked` :683, `.clean_on_escape()` :699, `.soft_wrap` :705,
  `.pattern(regex)` :737, `.validate(fn)` :759, `.default_value(...)` :775,
  `.mask_pattern(...)` :1795.
- Runtime: `set_value` :599, `value() -> SharedString` :787, `text() -> &Rope`
  :797, `insert` :633, `replace` :648, `set_placeholder` :553,
  `cursor_position`/`set_cursor_position` :802-823, `focus(window, cx)` :825,
  `set_loading` :768.
- Events: `impl EventEmitter<InputEvent> for InputState` (state.rs:337);
  `InputEvent { Change, PressEnter { secondary }, Focus, Blur }`
  (state.rs:93-98). Subscribe with `cx.subscribe(&input_state, ...)`.
  NOTE: 0.5.1 InputEvent::Change carries NO text payload (git HEAD does);
  read `state.value()` in the handler.
- `Input` element builders (input.rs): `.prefix(impl IntoElement)` :78 (prefix
  icon for palette box), `.suffix` :83, `.appearance(bool)` :101,
  `.bordered`/`.focus_bordered` :107-117, `.cleanable(bool)` :119,
  `.mask_toggle()` :125, `.disabled` :131, `.tab_index(isize)` :137,
  `.h/.h_full` :89-99.
- NumberInput (input/number_input.rs): wraps same InputState;
  `NumberInput::new(&Entity<InputState>)` :38, `.placeholder` :52,
  `.prefix/.suffix` :58-64; +/- buttons emit
  `NumberInputEvent::Step(StepAction::{Increment,Decrement})` on the
  InputState (:110-123). App handles stepping/parsing itself (pair with
  InputState::pattern for digits).
- Focused-input tracking exposed via WindowExt::focused_input (root.rs:205).

## 5. Menus

gpui-component (GC/src/menu/):
- `PopupMenu` (popup_menu.rs): `PopupMenu::build(window, cx, |menu, window, cx| ...)`
  :318 returning Entity<PopupMenu>; item builders `.menu(label, Box<Action>)`
  :375, `.menu_with_icon` :453, `.menu_with_check` :475, `.menu_element*`
  :497-534, `.separator()` :590, `.submenu` :604, `.link*` :408-426, `.label`
  :402, `.scrollable` :357, `.max_h` :349; `.action_context(FocusHandle)` :331
  routes dispatched actions to that focus handle. `PopupMenuItem` enum :30.
- Context menu: `ContextMenuExt` trait (context_menu.rs:13) adds
  `.context_menu(|PopupMenu, window, cx| PopupMenu)` :18 to any
  ParentElement+Styled element; right-click opens at pointer.
- Panel title-bar menu: override `Panel::dropdown_menu` (panel.rs:137).
- In-window menu bar for Windows/Linux: `AppMenuBar::new(window, cx)`
  (app_menu_bar.rs:25-48) builds from `cx.get_menus()` (i.e. from
  gpui set_menus data); keyboard nav bound in "AppMenuBar" context.

gpui 0.2.2 native menus (GP/src/):
- `App::set_menus(Vec<Menu>)` GP/src/app.rs:1840; `App::set_dock_menu` :1850.
- `Menu { name: SharedString, items: Vec<MenuItem> }` GP/src/platform/app_menu.rs:5.
- `MenuItem::{Separator, Submenu(Menu), SystemMenu(OsMenu), Action { name,
  action: Box<dyn Action>, os_action: Option<OsAction> }}` app_menu.rs:52-73;
  ctors `MenuItem::action(name, action)` :96, `os_action` :105,
  `separator` :78, `submenu` :83, `os_submenu(.., SystemMenuType::Services)` :88.
- macOS: real NSMenu implementation (GP/src/platform/mac/platform.rs:914).
  Linux/Windows: set_menus only stores OwnedMenu data (linux/platform.rs:474,
  windows/platform.rs:516) -- no native bar; use AppMenuBar there.

## 6. Notifications / toasts (GC/src/notification.rs + root.rs)

- Build: `Notification::info/success/warning/error(msg)` :141-162, or
  `Notification::new()` :115 + `.title` :188, `.message` :135, `.icon` :196,
  `.with_type(NotificationType)` :202, `.autohide(bool)` :208, `.on_click`
  :214, `.action(..)` :223, dedupe ids `.id::<T>()` :174 / `.id1::<T>(key)` :180.
- Show via `window.push_notification(note, cx)` (root.rs:169); list is
  `NotificationList` (notification.rs:364) rendered by
  `Root::render_notification_layer` (root.rs:279). Remove by type
  `remove_notification::<T>` (root.rs:178); `clear_notifications` :188.

## 7. Keybinding / action helpers beyond raw gpui

- `Kbd` component (GC/src/kbd.rs): renders a keystroke chip;
  `Kbd::binding_for_action(window, action) -> Option<...>` :43 and
  `binding_for_action_in(focus_handle, ...)` :64 look up the highest-precedence
  binding to display shortcut hints (e.g. in palette rows);
  `Kbd::format(&Keystroke)` :81 gives platform-style text (cmd symbols etc).
- Shared UI actions (GC/src/actions.rs): `Confirm { secondary }` (no_json),
  `Cancel`, `SelectUp/Down/Left/Right` in namespace `ui`; components bind them
  per key-context ("Input" state.rs:102+, "List" list.rs init, "Dialog"
  dialog.rs:23-26, "Root" tab/shift-tab focus cycling root.rs:16-24,
  "SearchPanel" input/search.rs:29-35, "AppMenuBar").
- Dock actions: `actions!(dock, [ToggleZoom, ClosePanel])` mod.rs:29.
- PopupMenu items can carry `Box<dyn Action>` and dispatch into a chosen
  focus context via `.action_context` (popup_menu.rs:331) -- this is the glue
  for menu -> gpui action routing.
- Root binds tab/shift-tab to window.focus_next/focus_prev (root.rs:388-394).

## 8. Layout persistence serde shapes (GC/src/dock/state.rs)

```
DockAreaState { version: Option<usize>,       // serde(default)
                center: PanelState,
                left_dock/right_dock/bottom_dock: Option<DockState> } // skip if None
DockState     { panel: PanelState, placement: DockPlacement,
                size: Pixels, open: bool }                     // state.rs:26-31
PanelState    { panel_name: String, children: Vec<PanelState>,
                info: PanelInfo }                              // state.rs:69-73
PanelInfo (tagged by field name, state.rs:100-112):
  "stack" -> { sizes: Vec<Pixels>, axis: usize (0=h, 1=v) }
  "tabs"  -> { active_index: usize }
  "panel" -> serde_json::Value        (opaque per-panel payload)
  "tiles" -> { metas: Vec<TileMeta { bounds: Bounds<Pixels>, z_index }) }
```
Reserved panel_name values written by the crate itself: "StackPanel",
"TabPanel", "Tiles" (see dump impls stack_panel.rs:44, tab_panel.rs:146,
tiles.rs:155 and fixture tests/fixtures/layout.json).

App obligations to restore a layout:
1. Give every custom panel a stable `panel_name` (panel.rs:59).
2. Override `Panel::dump` to stash per-panel data as
   `PanelInfo::Panel(json)` (panel.rs:156, state.rs:126).
3. At startup call `register_panel(cx, name, closure)` for every name
   (panel.rs:356); closure receives `&PanelState` + `&PanelInfo` and returns
   `Box<dyn PanelView>`. Unregistered names render InvalidPanel.
4. Subscribe to `DockEvent::LayoutChanged` on the DockArea entity, debounce,
   call `dock_area.dump(cx)` and serde_json it (mod.rs:31-36, 927).
5. On boot, deserialize DockAreaState and call `dock_area.load(state, window,
   cx)` inside a window update; compare `state.version` against your expected
   default-layout version yourself (load just copies it, mod.rs:904).

## 0.5.1 vs git-HEAD deltas that matter for M2

- `ContextModal` naming from HEAD docs is `WindowExt` here; modal is `Dialog`
  (HEAD renamed Modal -> Dialog earlier; 0.5.1 already uses Dialog/Sheet).
- InputEvent::Change has no payload at 0.5.1 (HEAD: Change(SharedString)).
- DockEvent has only LayoutChanged + DragDrop; no richer events.
- No fuzzy matcher anywhere; HEAD examples' palettes also roll their own.
- No examples ship in the package; the repo examples referenced by earlier
  research are git-only and may target newer APIs.
- DockItem::split_with_sizes double-add quirk (mod.rs:221-231) is benign but
  do not rely on add order side effects.

## Task 2 field corrections (verified building the workbench)

- A BARE center Tabs never emits DockEvent::LayoutChanged. `set_center`
  (mod.rs:596) calls `subscribe_item`, whose `DockItem::Tabs` arm is a no-op
  (mod.rs:979-981: "We subscribe to the tab panel event in StackPanel's
  insert_panel"). Only a TabPanel inserted INTO a StackPanel gets subscribed
  (stack_panel.rs:237-244 -> DockArea::subscribe_panel -> re-emits
  DockEvent::LayoutChanged on PanelEvent::LayoutChanged). Practical rule:
  keep the center a Split(StackPanel) wrapping the Tabs, and never call
  set_center for a bare Tabs -- build the first tab via
  `DockArea::add_panel(_, Center, ..)` onto the initial empty Split created by
  `DockArea::new` (that Split's StackPanel is subscribed in `new`, mod.rs:548).
  Then tab switches (set_active_ix, tab_panel.rs:234), adds, removes, and
  closes all reach a DockEvent::LayoutChanged subscriber.
- `DockArea::load` (mod.rs:898) does NOT call `subscribe_item` on the restored
  center; it relies on `to_item` -> `split_with_sizes` -> StackPanel inserts
  subscribing each child TabPanel. A saved bare-Tabs center would restore
  unsubscribed. Since dumps of a Split center round-trip as a StackPanel with
  TabPanel children, the "always a Split" rule above also keeps restore
  subscribed.
- The `items: Vec<Arc<dyn PanelView>>` cached inside `DockItem::Tabs` is NOT
  authoritative: `DockArea::add_panel(Center)` routed through
  `DockItem::Split::add_panel` (mod.rs:410-419) updates only the TabPanel
  entity, and `DockItem::Tabs::remove_panel` (mod.rs:452-457) never pops from
  it. Enumerate live tabs via `dump()` (TabPanel::dump reads its real panels)
  or `TabPanel::active_panel`, never by walking `DockItem::Tabs.items`.
- STALE Split.items after auto-removal (bit us hard). When a TabPanel's last
  panel is removed, `remove_self_if_empty` (tab_panel.rs:351) tells its parent
  StackPanel to drop it (`StackPanel::remove_panel`, stack_panel.rs:274-291).
  That updates the LIVE tree (StackPanel.panels, and what dump()/render walk),
  but NOT the cached `items: Vec<DockItem>` inside the parent
  `DockItem::Split`/`DockItem::Tabs` held by `DockArea.items` -- that vec is
  only rebuilt by `new`/`load`/`set_center` (mod.rs:598,919). So after closing
  to zero tabs, `DockArea::add_panel(_, Center, ..)` routes through
  `DockItem::Split::add_panel` (mod.rs:411-420), finds the STALE detached
  TabPanel in `items`, and adds into it -> the panel never renders and dump()
  never sees it; every later center add vanishes too. Fix: when reducing the
  center to empty, do not reuse it -- rebuild via `set_center(DockItem::split(
  axis, vec![], &weak, window, cx))` (fresh, re-subscribed StackPanel) then
  `add_panel`. Verified: routing an add through the post-auto-removal cache is
  a dead panel.
  - NOT only close-to-zero. A user drag-to-split (`TabPanel::add_panel_at`,
    tab_panel.rs:280) grows the LIVE StackPanel but also never updates
    `DockArea.items`; emptying one pane then detaches its TabPanel while the
    cached `Split.items` keeps the orphan, and a later center add with
    file_count still > 0 can route into it. General fix in hxy-gpui: detect a
    cached `DockItem::Tabs` whose live `TabPanel::active_panel(cx)` is `None`
    (emptied-but-not-removed cache entry) and, before any center add, resync the
    cache to the live tree by mirroring `dump()`'s `PanelState` structure into a
    fresh `DockItem` tree installed with `set_center`. CRITICAL: do NOT let
    `PanelState::to_item` call `PanelRegistry::build_panel` for the file leaves --
    that constructs FRESH `FilePanel`s (re-reads from disk) and DISCARDS live
    editor state (dirty edits, vim mode, scroll). Instead keep your own registry
    of open `FilePanel` entities (keyed by path) and reinstall the EXISTING
    entities via `DockItem::tabs`/`split_with_sizes`, only falling back to
    `build_panel` when no live entity exists (genuine boot restore). This
    preserves the split structure (no flatten), never disables drag-to-split, and
    keeps editor state intact. Registry caveat: prune it lazily (dedup per path on
    open, reset from the rebuilt cache after a resync) -- a per-reconcile prune by
    "path present in dump()" WRONGLY drops a file that is transiently absent from
    the tree while an async `add_panel_at`/`split_panel` is mid-flight, losing the
    reuse handle. Keep the empty/welcome case on the single-`dock.update`
    fresh-Split factory (below); use the resync only for the >0-survivor case.
  - `set_center` vs `load` for a LIVE rebuild: both reduce to
    `PanelState::to_item`, but `DockArea::load` (mod.rs:898) does NOT
    `subscribe_item` the rebuilt center, so its new StackPanel never re-emits
    `LayoutChanged` and the app's dock-event subscription goes deaf. `set_center`
    (mod.rs:596) does. Also: an EMPTY `set_center(to_item(empty))` done in a
    separate `dock.update` from the follow-up `add_panel` failed to wire the
    child subscription (StackPanel insert subscribes via `window.defer`,
    stack_panel.rs:222); doing fresh-Split `set_center` + `add_panel` in ONE
    `dock.update` (the factory path) works. Hence: factory-in-one-update for the
    empty case, entity-reusing rebuild + `set_center` for the
    collapse-with-survivor case.
- Delegating a Panel's `Focusable::focus_handle` to an inner child handle is
  safe even though `TabPanel::render` also `track_focus`es the same handle
  (tab_panel.rs:1191, focus_handle = active_panel.focus_handle). gpui's
  `set_focus_id` (key_dispatch.rs:219-222) inserts focus_id -> node with
  last-writer-wins, and the child paints after the TabPanel wrapper, so the
  child node wins `focusable_node_ids`; the focus path ends at the child and
  its `on_key_down` fires. Verified end-to-end: keystrokes reach the inner
  HexPane through the dock with no extra handler.

## Task 6 field corrections (menus, actions, toasts)

- Native menu shortcut text is auto-derived, not `os_action`: macOS's
  `create_menu_item` (GP/src/platform/mac/platform.rs:305-323) looks up
  `keymap.bindings_for_action(action)` and shows that keystroke next to the
  item. So a plain `MenuItem::action(name, MyAction)` picks up whatever
  `cx.bind_keys` registered for `MyAction` automatically -- no need to also
  pass `MenuItem::os_action` unless you want the item to fall through to a
  native `NSResponder` selector (`cut:`/`copy:`/`paste:`/`selectAll:`; `Undo`/
  `Redo` os_actions are dead code per that file's own comment: "always
  disabled ... we don't have a NSTextView/NSTextField to enable them on").
  hxy-gpui's menu items all use plain `MenuItem::action`, since every one has
  its own real `on_action` handler.
- Dynamic menu enable/disable exists mechanically but hxy-gpui's binding
  strategy makes it a no-op. `App::is_action_available` (app.rs:1824, called
  by the platform's `on_validate_app_menu_command`) walks the *currently
  focused element's* dispatch path for a registered `on_action` listener
  (key_dispatch.rs:381-393) -- it does not know or care whether that handler
  would actually do anything. Every hxy-gpui menu handler (Undo/Redo/Copy
  Bytes/Copy Hex/Close Tab/Toggle Edit Mode/...) is bound once on
  `Workspace`'s root `div`, which sits on every focus path in the window
  regardless of editor state. So every item reports "available" all the
  time; there is no grey-Undo-when-history-is-empty behavior. Getting real
  semantic disable would mean conditionally attaching/detaching the
  `on_action` listener based on state (e.g. only bind `Undo` on the div when
  `editor.can_undo()`), which M2 does not do -- every handler instead
  no-ops cleanly on empty state (see `Workspace::on_undo` etc.).
- `App::dispatch_action` (app.rs:1879) routes through the *active window's*
  focused-dispatch-path when one exists, and only falls back to
  `global_action_listeners` when there is no active window. But
  `Window::dispatch_action_on_node` (window.rs:3992) checks
  `global_action_listeners` in BOTH a capture pass (before window/element
  `on_action`s) and, if nothing along the window path stopped propagation, a
  bubble pass after them (window.rs:4064-4085). So `cx.on_action::<Quit>(...)`
  registered globally on `App` still fires correctly even with a window
  focused, as long as nothing on the focused dispatch path also handles
  `Quit` (nothing in hxy-gpui does).
- `WindowExt::push_notification` (`Root::update`, root.rs:169) panics if the
  window's actual root view isn't yet a `gpui_component::Root` -- true during
  `Workspace::new`/`build_initial` (main.rs wraps `Workspace` in `Root` only
  *after* `Workspace::new` returns), and true again if you're already inside
  a `Root::update` closure (a second nested `Root::update` double-borrows the
  same entity and panics: "cannot update ... while it is already being
  updated"). Fixes used here: (1) boot-time toasts (the layout-restore
  warning) go through `window.defer(cx, ...)`, which runs after the current
  update finishes -- by then `Root` is installed; (2) tests that need a real
  `Root` build one explicitly (`open_workspace_with_root` in
  `workspace.rs`'s tests, mirroring the `DialogTestHost` pattern already used
  in `search_bar.rs`/`file.rs`), and drive it via
  `AppContext::update_window` (gives `window`/`cx: &mut App` without
  entity-borrowing the root view) rather than `WindowHandle<Root>::update`
  (which pre-borrows `Root`, so a `push_notification` inside the callback
  double-borrows it).
