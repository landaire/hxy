# M4 reference: hxy-gpui architecture map (verified 2026-08-20)

Working notes for M4 implementers. File:line refs verified against the rebased tip; where stale, current source wins.

## Crates

`gpui/hxy-gpui/` (shell/workbench) and `gpui/hxy-view-gpui/` (hex-view widget). Shell built on gpui-component dock (`DockArea`, `Panel`, `PanelRegistry`). All user-facing strings via `hxy_i18n::t`/`t_args`. Business logic in shared `hxy_*` crates; panels are thin adapters.

## Workspace root and dock (workspace.rs, ~6200 lines)

- `pub struct Workspace` (workspace.rs:233). main.rs:58 wraps it in `gpui_component::Root::new(...)` which owns dialog/sheet/notification layers.
- Fields: `dock: Entity<DockArea>` (:234, created with `DockArea::new("workspace", Some(persist::LAYOUT_VERSION), window, cx)` :384); `welcome: Option<Entity<WelcomePanel>>`; `active_file: Option<Entity<FilePanel>>` (strict active tab) + `last_active_file` + `reference_active_file()` (:2549, FILE-scoped fallback); registries `open_files: Vec<Entity<FilePanel>>`, `strings_panels`, `strings_panel_subs: Vec<Subscription>`, `entropy_panels`, `checksums_panels`, `global_search_panel`; overlays `palette: Entity<Palette>`, `pane_picker: Entity<DockPicker>`; `layout_path`, `save_debounce`, `_watch_poll_task`, `file_watch`; dialog queues `pending_reload/close/restore`, `restore_queue`, `closed_tabs` (cap 32, :130); flags `needs_reconcile`, `focus_pending`.
- Constructor `new(initial: Vec<PathBuf>, appearance_subscription: Subscription, layout_path: Option<PathBuf>, window, cx)` (:377). Subscribes DockEvent (:385): LayoutChanged -> needs_reconcile + schedule_save + notify. `build_initial` (:451) loads persisted layout, prunes, opens CLI paths, ensure_inspector_dock, reconcile.
- Add-panel idiom (repeated ~15x):
  `self.resync_center_if_stale(window, cx); let view: Arc<dyn PanelView> = Arc::new(panel); self.dock.update(cx, |dock, cx| dock.add_panel(view, DockPlacement::Center, None, window, cx));`
- Side docks: `dock.set_right_dock(item, Some(width), open, window, cx)` (:587); `dock.toggle_dock(DockPlacement::Right, window, cx)` (:591). `DockItem::tabs(...)`/`DockItem::split(...)` build subtrees.
- GOTCHA: DockArea `items` is a cache rebuilt only in new/load/set_center; drag-splits desync it. `dump(cx)` walks the LIVE tree. Healing: `resync_center_if_stale` (:1155) -> `rebuild_center_cache` (:1202) mirrors dump().center via `rebuild_item` (:2817)/`resolve_leaf` (:2869), REUSING FilePanel entities by path (others rebuilt; registries + subs re-tracked). `center_cache_missing_a_live_leaf` (:1182) used by pane picker.
- `reconcile` (:2460): has_content from dump(); empty -> fresh Split + WelcomePanel; else remove welcome; set_active_file(active_file_panel(...)); republishes `cx.set_global(OpenFilePanels(...))` (:2513).
- Cross-panel communication via App globals: `ActiveHexPane(Option<Entity<HexPane>>)` published by `publish_reference_active_file` (:2590); `OpenFilePanels(Vec<Entity<FilePanel>>)` from reconcile; `ActiveInspectorPanel`; `VfsRegistryGlobal`.
- Reconcile/reconstruction deferred out of render via `window.defer(cx, ...)`.
- Registry helpers: `collect_file_entities`/`collect_strings_entities`/...; `active_file_panel`/...; `count_file_panels`/`dump_has_file_path`/`dump_has_strings_path`/...

## Panel registry and persistence

- `panels/mod.rs`: `pub const *_PANEL_NAME` per panel; `pub fn register(cx: &mut App)` (mod.rs:48) calls `register_panel(cx, NAME, |dock, state, info, window, cx| Box::new(cx.new(|cx| Panel::restore(info, window, cx))))` per panel + `workspace_host::register(cx)`. Registered names (stable forever): FilePanel, WelcomePanel, InspectorPanel, StringsPanel, EntropyPanel, ChecksumsPanel, ComparePanel, GlobalSearchPanel, WorkspaceHostPanel. Test `every_registered_name_builds_a_real_panel` guards fallback.
- `persist.rs`: `LAYOUT_VERSION: usize = 1` (:27); `layout_path()` -> `$DATA_DIR/hxy/gpui-dock-layout.json`; `snapshots_base()` -> `$DATA_DIR/hxy/gpui/snapshots`; `load/save`; `prune_for_restore(&mut DockAreaState) -> PrunedTabs` (:185) driven by `panel_kind(name) -> Option<PanelKind>` (:264). PanelKind: File (kept if path readable), Welcome (never restored), OwningPathLeaf{log_label} (kept only if owning file survives), Compare (both sides readable), WorkspaceHost (archive readable), AlwaysKeep (global search). Adding a per-file secondary panel = one panel_kind arm. Handles gpui-component 0.5.1 empty-container Panel(null) quirk; resets fully-pruned center to empty_center().

## Add-a-panel pattern (reference: inspector.rs, strings.rs)

Every panel: Entity implementing `Panel` + `Focusable` + `EventEmitter<PanelEvent>` + `Render`.
- Panel trait surface used: `panel_name() -> &'static str` (stable const); `title(window, cx) -> impl IntoElement` (SharedString from t(...)); `tab_name(&self, cx) -> Option<SharedString>`; `closable(&self, cx) -> bool` (only veto point for tab-bar close); `dump(cx) -> PanelState` (`PanelState::new(self)` + `PanelInfo::panel(serde_json::json!({...}))`); `on_removed(window, cx)` cleanup.
- Focus: stored `FocusHandle` or delegate to inner pane (FilePanel returns pane's).
- Two constructors: `new(...)` (fresh, binds immediately) and `restore(info: &PanelInfo, window, cx)` (defers binding); both -> private `with_state(...)`.
- Per-file rebinding: store `owning_path: Option<PathBuf>`; `cx.observe_global::<OpenFilePanels>(|this, cx| this.try_bind_from_global(cx))` (strings.rs:183); `try_bind_from_global` (:227) -> `bind_pane`. Inspector mirror: `cx.observe_global::<ActiveHexPane>` + re-`cx.observe(pane, ...)` per active pane (inspector.rs:127,145).
- Background compute (strings.rs::run :369): `running: bool` + `pending_rerun: bool` guard; `cx.spawn(async move |this, cx| { let outcome = cx.background_spawn(async move { ... }).await; this.update(cx, |this, cx| { apply; replay pending }) })`; Task stored in `self._compute`. `recompute_after_reload` (:414) gated on `AUTO_RUN_MAX_BYTES = 256 MiB`.
- Panel->workspace events: zero-size event struct (e.g. `pub struct StringsJumped;` + `impl EventEmitter<StringsJumped> for StringsPanel` strings.rs:697); workspace `cx.subscribe_in(&panel, window, Self::on_...)` stored in subs vec.
- Widgets: `Button::new(id).label(...).compact().selected(...).on_click(cx.listener(...))`, `Input::new(&input_state)`, `Table::new(&table_state)` + TableDelegate, `Label::new(...)`, `h_flex()`/`v_flex()`. Theme via `cx.theme().border/.background/.muted_foreground/.primary`.

## Async patterns

- Off-thread compute: `cx.background_spawn(...)` inside `cx.spawn(...)`; Task kept in field; None cancels.
- Poll loop: `spawn_watch_poll` (workspace.rs:2897): `cx.spawn_in(window, async move |this, cx| loop { gpui::Timer::after(POLL_INTERVAL).await; this.update_in(cx, ...)?; })`.
- Debounced save: `schedule_save` (:2602) replaces `save_debounce` task; `SAVE_DEBOUNCE = 500ms`.
- Watcher wake (watch.rs): mpsc channel; Wake callback sends (); `poll(live_paths)` diffs HashSet -> watch/unwatch, drains receiver, returns Vec<WatchEvent>; `apply_reload(file, path, decision, cx)` re-reads disk + swap_source(_keep_patch). Keyed by gpui::EntityId via hxy_panels::watch aliases.
- Deferred UI: `window.defer(cx, ...)` for anything needing &mut Window post-frame or Root layers.

## Menu / palette / commands

- Actions: `gpui::actions!(hxy_gpui, [OpenFile, Save, SaveAs, ReopenClosedTab, ToggleVim, ToggleInspector, ToggleSearch, CloseSearch, ToggleGlobalSearch, OpenPalette, PickPane, OpenStrings, OpenEntropy, OpenChecksums, TakeSnapshot, OpenSnapshots])` (workspace.rs:106); `hxy_gpui_menu` (menu.rs:48): ShowAbout, Quit, CloseTab, Undo, Redo, ToggleEditMode, CopyBytes, CopyHex; `hxy_gpui_palette` (palette/mod.rs:65): PaletteUp/Down/Dismiss.
- Keybindings: `workspace::init_keybindings(cx)` (:204) + `menu::init_keybindings(cx)` (:54) via `cx.bind_keys([KeyBinding::new("cmd-o", OpenFile, None), ...])`; scope name (e.g. Some("Palette")) restricts to elements with matching `.key_context(...)`. `menu::init_global_actions(cx)` -> `cx.on_action(|_: &Quit, cx| cx.quit())`. main.rs calls all + `cx.set_menus(menu::build_menus())`.
- Handlers on Workspace root div in render via `.on_action(cx.listener(Self::on_open_file))` (~22, :3432-3452). `menu::build_menus() -> Vec<Menu>` of `MenuItem::action(t(key), Action)`.
- Palette (palette/mod.rs): custom top-center overlay, always childed by Workspace render, opened by OpenPalette (cmd-shift-p). `Palette { workspace: WeakEntity<Workspace>, state: palette_core State, mode: PaletteMode, input: Entity<InputState>, restore_focus, compare_a, ... }`. Rows via `palette_core::filter_and_sort`. Picks -> `palette::apply::apply(ws, action, window, cx)`.
- palette/modes.rs: `PaletteMode` (Main, GoToOffset, SelectFromOffset, SelectRange, SetColumns, CompareSideA/B, ...) with `parent()`, `bypasses_filter()`, `hint_key()`. `PaletteAction` (OpenFile, CloseTab, ToggleVim, ToggleInspector, ToggleGlobalSearch, OpenStrings, OpenEntropy, OpenChecksums, BrowseVfs, SwitchMode, GoToOffset(u64), SetSelection{...}, SetColumns, CopyText, CopySelection, CompareSelectSource{...}, CompareBrowse, NoOp). `build_entries(mode, query, ctx: PaletteContext, shortcuts) -> Vec<Entry<PaletteAction>>` (:201) pure + unit-tested; `PaletteContext` (:171) filled by `palette_context(cx)` (workspace.rs:2065).
- To add a palette command: PaletteAction variant -> Entry in `build_main_entries` (modes.rs:222) -> match arm in apply.rs:24 routing to a Workspace method.

## Status bar and toasts (status.rs)

- Pure formatting fns (unit-tested): `window_title_text`, `status_file_name_text`, `status_offset_text`, `status_vim_mode_text`, `dirty_marker`, `status_open_error_text`. Wired in `render_status_bar` (workspace.rs:2650); window title set in render (:3392) when changed (last_title).
- Toasts: `window.push_notification(Notification::error/warning/info(text), cx)`; require Root; boot-time toasts deferred. Root layers appended at end of Workspace render: `Root::render_dialog_layer` / `render_notification_layer` (:3475). Dialogs: `window.open_dialog(cx, |dialog, window, cx| ...)`; footer closures upgrade weak + update (reload_button :2913).

## hxy-view-gpui public API

- lib.rs re-exports: HexPane, FrameInfo, ByteStyler, ByteStyleOverride, MinimapBounds, geometry (GridGeometry, GridHit, CellMetrics).
- `ByteStyleOverride { bg: Option<Hsla>, fg: Option<Hsla> }` (pane.rs:56); `ByteStyler = Arc<dyn Fn(u8, ByteOffset) -> ByteStyleOverride + Send>` (pane.rs:66).
- Setters (each cx.notify()): `set_byte_styler(Option<Box<dyn Fn(u8, ByteOffset) -> ByteStyleOverride + Send>>, cx)` (:193); `set_hover_span(Option<ByteRange>, cx)` (:172) + `hover_span()`; `set_row_map(Option<Vec<RowSlot>>, cx)` (:164); `set_source` clears all three.
- paint.rs::paint_grid (:115) per-row order (:264-269): hover band -> selection bands -> styler tints -> cursor cell -> row text -> nibble caret; header last. `paint_styler_tints` (:565) skips selection/hover-covered cells, `merge_tint_runs` (:589). `paint_row_text` (:521): styler fg wins. `GridSnapshot` (:82) captured in HexPane::render (pane.rs:546): source, selection, active_pane, nibble, columns, scroll_rows, `colors: PaintColors::from_theme(cx.theme())` (:662, hover = selection.opacity(0.45)), mono_family/mono_size, row_map, hover_span, byte_styler. `hex_canvas(snap, entity)` (:104) writes FrameInfo back + `editor_mut().on_frame(...)`.
- HexPane API used by panels/file.rs: `editor()`/`editor_mut()` (selection, source, is_dirty, set_selection, set_scroll_to_byte, splice, swap_source(_keep_patch), set_edit_mode, reset_edit_nibble, input_mode, vim_state); `sync_pending_scroll(cx)` (pane.rs:520) MUST follow programmatic selection moves; `set_columns(ColumnCount, cx)` (:143); `focus_handle(cx)`; `last_frame()`; `scroll_rows()`/`set_scroll_rows(f32, cx)`.
- FilePanel (file.rs:42): owns `pane: Entity<HexPane>` (built :61), `search: Entity<SearchBar>` (childed below pane, render :317), `detected_handler: Option<Arc<dyn VfsHandler>>`, lazy `snapshots: Option<SnapshotStore>`.

## Theming / fonts

No settings module. `main.rs`: `Theme::sync_system_appearance(Some(window), cx)` (:43) + `window.observe_window_appearance(...)` (:44) -> subscription stored in Workspace as `_appearance_subscription`. Colors via `cx.theme()` (gpui_component ActiveTheme). Mono font: `cx.theme().mono_font_family` / `.mono_font_size`, consumed in HexPane::render/paint.rs cell_metrics.

## Nested docks (panels/workspace_host.rs)

WorkspaceHostPanel owns inner `Entity<DockArea>` ("workspace-inner"): VfsTreePanel in inner left dock + FilePanel per opened entry in inner center. Holds `outer_dock: WeakEntity<DockArea>`; subscribes inner LayoutChanged -> needs_guard -> render defers `enforce_membership` (:338) ejecting non-`owned` panels to outer dock + warning toast. `dump` hand-composes nested layout (parent_path, expanded, entry paths, virtual_bases) into one PanelInfo JSON; restore re-mounts + re-opens. `register(cx)` (:100) installs VfsRegistryGlobal + registers panel. `collect_active_panels(item, cx, out)` (:552).

## Recipe: new dockable panel + palette command + persistence

1. `panels/foo.rs`: `FOO_PANEL_NAME` const + struct (focus_handle, owning_path, owning_pane, _rebind_observe, running/pending_rerun/_compute, last_result, widgets).
2. Constructors `new`/`restore` -> `with_state` installing OpenFilePanels observer; `try_bind_from_global` + `bind_pane` (auto-run under AUTO_RUN_MAX_BYTES); `*_from_info` defaults-missing/warns-corrupt.
3. Impl Panel (panel_name/title/tab_name/closable/dump/on_removed), Focusable, EventEmitter<PanelEvent>, Render; optional FooJumped event.
4. Background compute per strings.rs::run.
5. panels/mod.rs: decl + register_panel line.
6. persist.rs: one panel_kind arm.
7. workspace.rs: registry vec field; open_foo_for_active_file (mirror open_strings_for_active_file :717); focus/close/collect/active/dump_has/count helpers; is_known_panel_name arm; refresh in build_initial + rebuild_center_cache.
8. Action + keybinding + menu item + on_action handler.
9. PaletteAction variant + entry + apply arm.
10. i18n keys.
11. Tests: registered-name build test auto-covers; add prune tests + dump round-trip.
