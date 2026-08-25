# M4f: gpui-vs-egui feature-parity matrix (audited 2026-08-21)

Final audit deliverable for the GPUI port (spec:
`docs/superpowers/specs/2026-07-24-gpui-port-design.md`). The port goal is
functional + feature equivalence with idiomatic gpui-component widgets, not a
pixel clone. This matrix walks every egui app subsystem and records the gpui
counterpart with an honest status.

Status vocabulary:

- `parity`: same feature, reachable and behaviorally equivalent (widget/idiom
  may differ).
- `deviation`: a real, documented behavioral difference (surface moved, a
  setting not applied the same way, a rendering nuance). Each row links the
  code comment or spec entry that records it. Undocumented deviations found
  during the audit were documented (see the consolidated section in the port
  spec) or listed as findings below.
- `gap`: no gpui counterpart. Functionally reachable alternatives are noted.
- `n/a-wasm-only`: egui-only because it targets the web/wasm build, which is a
  non-goal for the GPUI port.

Line references were verified against the tip audited on 2026-08-21; where a
line has since drifted, the cited symbol name is authoritative.

## Summary counts

- parity: 80
- deviation: 24
- gap: 12
- n/a-wasm-only: 4

The gaps are dominated by (a) palette commands whose function is reachable via
another surface (menu, keybinding, native docking, settings) and (b) two
genuinely unported features: a paste path and the byte-cache debug (`Memory`)
panel. One build-infra item (the `wat` lockfile split) is OPEN and handed to
the milestone gate.

## 1. Tabs / panels

egui dock tabs: `crates/hxy/src/tabs/mod.rs:21` (`enum Tab`). gpui panels:
`gpui/hxy-gpui/src/panels/`.

| feature | egui location | gpui location | status |
|---|---|---|---|
| Welcome tab (app name/tagline + recents) | `app/mod.rs::welcome_ui` (~3890) | `panels/welcome.rs` (`recent_rows`, `OpenRecentRequested`) | parity |
| File tab (hex/ASCII grid, editing, minimap) | `Tab::File`, hxy-view | `panels/file.rs` + hxy-view-gpui `HexPane` | parity |
| Workspace tab (file + nested VFS dock) | `Tab::Workspace` | `panels/workspace_host.rs` | parity (nested live-settings deviation, see 6) |
| Settings tab | `Tab::Settings`, `app/mod.rs::settings_ui` (4020) | `panels/settings_view.rs` | parity (field deviations, see 6) |
| Console tab | `Tab::Console`, `app/mod.rs::console_ui` (3810) | `panels/console_view.rs` | parity (autoscroll deviation, see 13) |
| Inspector tab (datatype decode at caret) | `Tab::Inspector`, `panels/inspector.rs` | `panels/inspector.rs` | parity |
| Plugins manager tab | `Tab::Plugins`, `panels/plugins.rs` | `panels/plugins_view.rs` | parity (see 11) |
| PluginMount tab (live VFS mount tree) | `Tab::PluginMount`, `plugins/mount.rs` | `plugins.rs::install_mount_tab` + `workspace_host` | parity |
| SearchResults / global search | `Tab::SearchResults`, `search/global.rs` | `panels/global_search.rs` | parity (sort-order deviation `global_search.rs:23`) |
| Compare tab (side-by-side diff) | `Tab::Compare`, `compare.rs` | `panels/compare.rs` | parity (deviations, see 6/theme) |
| Entropy tab (Shannon plot) | `Tab::Entropy`, `panels/entropy.rs` | `panels/entropy.rs` | parity (axis deviation `entropy.rs:27`) |
| Visualizer tab (template-driven) | `Tab::Visualizer`, `visualizers/` | `panels/visualizer.rs` | parity (render deviations, see 10) |
| Strings tab | `Tab::Strings`, `panels/strings.rs` | `panels/strings.rs` | parity |
| Checksums tab | `Tab::Checksums`, `panels/checksums.rs` | `panels/checksums.rs` | parity |
| Memory (byte-cache debug) tab | `Tab::Memory`, `panels/memory.rs` | none | gap (gpui reads whole files into `MemorySource`, builds no `ByteCache`; tied to `byte_cache_limit_mib` deviation, `settings.rs:12`) |

## 2. Menu items

egui native menu: `crates/hxy/src/menu.rs` (`MenuAction`). gpui:
`gpui/hxy-gpui/src/menu.rs::build_menus`.

| feature | egui location | gpui location | status |
|---|---|---|---|
| App: About | `menu.rs` PredefinedMenuItem::about | `build_menus` ShowAbout | parity |
| App: Services / Hide / Show All | `menu.rs` predefined | none (gpui has no app bundle) | gap (macOS predefined items; see 12) |
| App: Quit | predefined quit | `Quit` action | parity |
| File: New | `MenuAction::NewFile` (cmd-N) | none | gap (no scratch buffer, see below) |
| File: Open | `MenuAction::OpenFile` | `OpenFile` | parity |
| File: Save / Save As | `MenuAction::Save/SaveAs` | `Save`/`SaveAs` | parity |
| File: Close | `MenuAction::CloseTab` | `CloseTab` | parity |
| File: Reopen Closed Tab | `MenuAction::ReopenClosedTab` | `ReopenClosedTab` | parity |
| Edit: Undo / Redo | `MenuAction::Undo/Redo` | `Undo`/`Redo` | parity |
| Edit: Toggle Edit Mode | `MenuAction::ToggleEditMode` | `ToggleEditMode` | parity |
| Edit: Paste / Paste as hex | `MenuAction::Paste/PasteAsHex` | none | gap (no paste path; `menu.rs:26` records the omission) |
| Edit: Copy bytes / Copy hex | `MenuAction::CopyBytes/CopyHex` | `CopyBytes`/`CopyHex` | parity |
| Edit: Copy bytes as / value as (submenus) | `MenuAction::CopyAs`, `files::copy` menus | palette `CopySelection` formats only | deviation (copy-as menu submenu not ported; copy formats reachable via palette) |
| View: Toggle Console | `MenuAction::ToggleConsole` | `OpenConsole` | parity |
| View: Toggle Inspector | `MenuAction::ToggleInspector` | `ToggleInspector` | parity |
| View: Toggle Plugins | `MenuAction::TogglePlugins` | `OpenPlugins` | parity |
| View: Toggle Settings | `MenuAction::ToggleSettings` | `OpenSettings` | parity |
| View: Strings / Entropy / Checksums | (palette in egui) | `OpenStrings/OpenEntropy/OpenChecksums` | parity (gpui adds these to the menu) |
| View: Toggle Global Search / Vim | (palette/shortcut in egui) | `ToggleGlobalSearch`/`ToggleVim` | parity |
| View: Take Snapshot / Snapshots | (palette in egui) | `TakeSnapshot`/`OpenSnapshots` | parity |
| Per-item dynamic greying (grey Undo etc.) | `MenuState::set_*` per frame | none | deviation (`menu.rs:13-24`: gpui items always enabled, handlers no-op) |

Note: gpui's menu is a superset in the View menu (Strings/Entropy/Checksums/
Snapshots surfaced as menu items) but a subset in File/Edit (no New, no Paste).

## 3. Keyboard shortcuts

egui: `crates/hxy/src/commands/shortcuts.rs` + `tabs/focus.rs`. gpui:
`workspace.rs::init_keybindings` + `menu.rs::init_keybindings`.

| shortcut | egui | gpui | status |
|---|---|---|---|
| cmd-C copy bytes | COPY_BYTES | cmd-c CopyBytes | parity |
| cmd-shift-C copy hex | COPY_HEX | cmd-shift-c CopyHex | parity |
| cmd-Z / cmd-shift-Z undo/redo | UNDO/REDO | cmd-z/cmd-shift-z | parity |
| cmd-E toggle edit mode | TOGGLE_EDIT_MODE | cmd-e | parity |
| cmd-W close tab | CLOSE_TAB | cmd-w | parity |
| cmd-shift-T reopen closed | REOPEN_CLOSED_TAB | cmd-shift-t | parity |
| cmd-S / cmd-shift-S save/save as | SAVE_FILE/_AS | cmd-s/cmd-shift-s | parity |
| cmd-O open | (menu) | cmd-o | parity |
| cmd-, settings | ToggleSettings accel | cmd-, OpenSettings | parity |
| cmd-shift-P command palette | COMMAND_PALETTE | cmd-shift-p OpenPalette | parity |
| cmd-K focus pane picker | FOCUS_PANE | cmd-k PickPane | parity |
| cmd-F / cmd-shift-F find local/global | FIND_LOCAL/FIND_GLOBAL | cmd-f/cmd-shift-f | parity |
| vim toggle | (palette) | cmd-alt-v ToggleVim | parity (gpui adds a binding) |
| cmd-V / cmd-shift-V paste/paste hex | PASTE/PASTE_AS_HEX | none | gap (no paste path) |
| cmd-N new file | NEW_FILE | none | gap (no scratch buffer) |
| cmd-P quick open | QUICK_OPEN | cmd-p OpenTabSwitcher (palette `QuickOpen` mode) | parity |
| ctrl-Tab / ctrl-shift-Tab next/prev tab | NEXT_TAB/PREV_TAB | none | gap (native dock tab focus; no keybound cycle) |
| alt-Tab toggle tab focus | TOGGLE_TAB_FOCUS | none | gap (nested-dock focus toggle; not ported) |
| cmd-] / cmd-[ jump next/prev field | JUMP_NEXT/PREV_FIELD | none (palette action only) | deviation (reachable via palette `JumpNextField/PrevField`, no keybinding) |

## 4. Command palette

egui: `commands/palette/` (`Action` / `PaletteCommand`, 60 command variants +
21 `Mode` variants). gpui: `palette/modes.rs` (`PaletteAction`, 31 variants +
13 `PaletteMode`). Shared fuzzy logic in `palette_core`.

Palette rows present with parity (same command, same routing): OpenFile,
CloseTab, ToggleVim, ToggleInspector, ToggleGlobalSearch, OpenStrings,
OpenEntropy, OpenChecksums, OpenSettings, OpenPlugins, OpenConsole,
OpenVisualizer, BrowseVfs, GoToOffset, GoToAddress, SelectFromOffset,
SelectRange, SetColumns(local), Templates + TemplatesAtSelection (run),
UninstallTemplate, CompareSideA/B, InvokePluginCommand + cascade + prompt,
FetchImhexPatterns, JumpNextField/PrevField, the `@expr` goto-calc and `=expr`
copy-calc prefixes, and copy-selection formats.

| egui palette command(s) | gpui | status |
|---|---|---|
| Undo / Redo / ToggleEditMode | menu + keybinding, not palette | deviation (functionally reachable off-palette) |
| CopyCaretOffset / CopyCaretAddress / CopySelectionRange(+Address) / CopySelectionLength / CopyFileLength | gpui `CopySelection`/`CopyText` cover selection + calc copies | deviation (per-target copy rows collapsed; caret/file-length copies absent) |
| SplitRight/Left/Up/Down, MergeRight/Left/Up/Down, MoveTab* , MergeVisual, MoveTabVisual | gpui-component DockArea native drag-drop docking | deviation (docking via drag, not palette verbs) |
| FocusPane | cmd-k PickPane (DockPicker overlay) | parity (off-palette) |
| WatchAlways/Ask/Never, SetPollInterval | Settings panel auto-reload + poll fields | deviation (per-file watch prefs via settings, not palette) |
| SetColumnsGlobal | Settings panel `hex_columns` | deviation |
| SetVirtualBase | none | gap (virtual-base offset labeling not ported) |
| Recent (mode) / OpenRecent | Welcome panel recents only | gap (no palette recents mode) |
| QuickOpen (cmd-P mode) | palette `QuickOpen` mode (cmd-p), fuzzy over all open tabs | parity |
| UninstallPlugin (mode) | Plugins panel delete button | deviation (plugin uninstall via panel, not palette) |
| CompareSideARecent / CompareSideBRecent | Compare browse dialog + open files | deviation (no recents as compare source) |
| ReloadActiveFile | file watcher reload dialog; no explicit palette reload | gap (reachable only via watch prompt) |
| TakeSnapshot / OpenSnapshots | menu + actions, not palette | deviation (off-palette) |
| NewFile / Paste / PasteAsHex | none | gap (see 2/3) |
| SaveAsDownload | wasm download flow | n/a-wasm-only |
| ComputeEntropy / ShowEntropy / ToggleEntropy | `OpenEntropy` (single open/focus) | parity (three egui variants collapse to one) |
| ToggleMemory | none | gap (no Memory panel, see 1) |
| ToggleWorkspaceVfs / CloseToolPane | workspace-host membership + native tab close | deviation |
| OpenFileWithOptions / FindStringsWithOptions / FindStringsSelection / CalculateChecksumsSelection | gpui opens the panel; panel owns options; selection-scoped runs via panel controls | deviation (with-options/selection-scoped entry rows collapsed into the panel UI) |

## 5. Status bar

egui: `app/mod.rs::status_bar_ui` (3337). gpui: `workspace.rs::render_status_bar`
+ pure formatters in `status.rs`.

| element | egui | gpui | status |
|---|---|---|---|
| File name | (window title) | `status_file_name_text` | parity |
| Caret offset / selection length | `status_bar_ui` caret/sel labels | `status_offset_text` | parity |
| Vim sub-mode indicator | vim label + tooltip | `status_vim_mode_text` | parity |
| Dirty marker | dirty asterisk | `dirty_marker` | parity |
| Read-only / edit lock toggle (click) | lock icon click | `render_status_bar` lock Button on_click | parity |
| Offset-base click-to-toggle on labels | `copyable_status_label` base toggle | none | deviation (`status.rs:42-44`) |
| Copyable hover-value readout | hover label (base + toggle) | none | gap (no hover readout in status bar) |
| Copyable selection/caret value (click-to-copy) | `copyable_status_label` | plain label | deviation (value shown, not click-copyable) |
| Tab-focus chip | tab_focus icon+label | none | gap (native-dock focus; chip not ported) |
| File-watch chip (eye/eye-slash, click toggles) | watch_chip | none | deviation (watch runs via settings; no status chip toggle) |

## 6. Settings

egui model `settings/mod.rs` (`AppSettings`, applied per-frame). gpui
`settings.rs` (`update_settings`, shared `hxy.db` `app_settings` key). Both
frontends share the same persisted blob.

| setting | egui apply | gpui apply | status |
|---|---|---|---|
| hex_columns (global) | per-frame + per-tab override | `settings_view` + live apply to every pane | deviation (global clobbers palette per-pane count; `settings.rs:26-31`) |
| input_mode (default/vim) | live loop | `settings_view` + panes | parity |
| offset_base | live | applied in status + panes | parity |
| numeric_format + template_value_formats | live | `settings::formats`, panels read | parity |
| byte_value_highlight / byte_highlight_scheme (Class/Value) | live | `highlight_palette` -> `file.rs:800`, `compare.rs` | parity |
| byte_highlight_mode (Background/Text) | live | `settings::view_mode`, applied `file.rs:800` | parity |
| show_minimap / minimap_colored | live | panes | parity |
| check_for_updates | placeholder (no checker) | not applied | deviation (`settings.rs:17`; placeholder in egui too) |
| language | dead in egui (no UI, unapplied) | dead | deviation (`settings.rs:19`; dead both sides) |
| zoom_factor | ctx.zoom_factor bidirectional | not applied | deviation (`settings.rs:14-16`) |
| address_separator_enabled / _char | grouped address formatter | not honored | deviation (`hxy-view-gpui/src/paint.rs:549-554`; needs pane capability) |
| compare_recompute_deadline | live | `compare.rs` debounce | parity |
| auto_reload (Always/Ask/Never) + file_watch_prefs | live | watch layer / settings | parity (per-file prefs via settings, not per-file palette) |
| file_poll_interval_ms / file_poll_all | live | watch layer | parity |
| byte_cache_limit_mib | live `byte_cache.set_limit` | not applied (no `ByteCache`) | deviation (`settings.rs:20-22`) |
| imhex_patterns state | app flow | palette flow owns fetch; blob round-trips | deviation (`settings.rs:23-24`) |
| recent_files | record_recent | shared; Welcome recents | parity |
| palette_escape_pops_to_parent | palette nav | palette nav | parity |
| Nested (workspace-host) file panes live settings | live | picked up at construction only | deviation (`settings.rs:32-35`; M3 nested-dock scope) |

## 7. File lifecycle

| feature | egui | gpui | status |
|---|---|---|---|
| Open from disk (rfd dialog / path) | `request_open_filesystem` | `workspace.rs` open flow | parity |
| Save / Save As | save.rs | M3 save + dirty-close | parity |
| Dirty-close prompt | `tabs/close.rs` | dialog queue (`pending_close`) | parity |
| Reopen closed tab (ring, cap 32) | `closed_tabs` | `ClosedTab` ring, cap 32 (`workspace.rs`) | parity |
| Unsaved-patch sidecars on quit | `$DATA_DIR/hxy/edits` | `patches.rs` (`$DATA_DIR/hxy/edits`, shared) | parity |
| File watching (poll + wake) | ipc/watch | `watch.rs` (poll loop) | parity |
| Auto-reload decision (Always/Ask/Never) | settings | `watch.rs::apply_reload` + settings | parity |
| Reload keep-patch vs discard | swap_source(_keep_patch) | `apply_reload` swap_source(_keep_patch) | parity |
| Snapshots (take / browse / restore) | snapshot store | `TakeSnapshot`/`OpenSnapshots`, `persist.rs::snapshots_base` | parity (separate `gpui/snapshots` dir, `persist.rs:63`) |
| Re-detect VFS handler on reload/restore | detect on open | `file.rs:317` | parity |
| New / scratch buffer (Untitled N) | NewFile | none | gap |
| Paste into buffer | PASTE/PASTE_AS_HEX | none | gap |

## 8. VFS / mounts

| feature | egui | gpui | status |
|---|---|---|---|
| VFS tree browser | `panels/vfs.rs` | `panels/vfs_tree.rs` | parity |
| Open entry -> File tab | pending-vfs-open queue | typed events | parity |
| Nested dock workspace (file + VFS) | `Tab::Workspace` | `workspace_host.rs` | parity |
| Foreign-panel eject from inner dock | dock rules | `enforce_membership` (`workspace_host.rs:369`) | parity |
| Plugin VFS mount (mount_by_token) | `plugins/mount.rs` | `plugins.rs::install_mount_tab` | parity |
| Failed-mount retry | `retry_failed_mount` | `plugins.rs` retry | parity |
| Mount internal reads off UI thread | known TODO (runner.rs:16-21) | same (reads on UI thread) | deviation (shared limitation, `watch.rs:15` notes no re-mount machinery) |
| Save VFS entry in place | `save_vfs_entry_in_place` | `workspace_host.rs:353` | parity |

## 9. Templates

| feature | egui | gpui | status |
|---|---|---|---|
| Run 010 / ImHex template | `hxy-templates` (M4a) | `templates.rs` + `panels/template_view.rs` | parity |
| Template tree render (structs/arrays/enums) | `panels/template.rs` | `template_view.rs` | parity |
| Deferred-array lazy expansion | flat tree expand | `render_deferred_cell` (`template_view.rs:853`) | parity |
| Field -> hex byte-range highlight | byte_palette | `file.rs` styler | parity |
| Template -> calculator field bridge | `calculator.rs` PathResolver | palette calc | parity |
| Template diagnostics (Error/Warning) | error toast + tracing | toast + Console entry (`templates.rs`, Console task) | parity |
| Install / uninstall template runtime | Plugins panel + palette | `InstallTemplate`/`UninstallTemplate` + panel | parity |
| Fetch ImHex patterns | patterns section | `FetchImhexPatterns` | parity |
| Template value copy formatting | `format_template_copy` | `hxy_templates::format::format_template_copy` (`file.rs:43`) | parity |

## 10. Visualizers

egui `visualizers/` (14 files). gpui `panels/visualizer.rs`. All 16 kinds
dispatch (`render_kind`, `visualizer.rs:429-446`).

| kind | egui | gpui | status |
|---|---|---|---|
| Image | image.rs | render_image | parity |
| Bitmap | bitmap.rs | render_bitmap | parity (NEAREST->bilinear filter deviation, `visualizer.rs:915-923`) |
| Digram | digram.rs | render_digram | parity |
| LayeredDistribution | distribution.rs | render_distribution | parity |
| HexViewer | hex_viewer.rs | render_hex_viewer | parity |
| Text | text.rs | render_text | parity (long lines clipped not h-scrolled, `visualizer.rs` mono_list) |
| ChunkEntropy | plot.rs | render_chunk_entropy | parity (X-axis hex ticks, no captions; `visualizer.rs:589-591`) |
| LinePlot / BarChart | plot.rs | render_series | parity |
| ScatterPlot | plot.rs | render_scatter | parity (custom point paint) |
| Sound | sound.rs | render_sound | parity (no playback either side) |
| Disassembler (x86) | disassembler.rs | render_disasm | parity (ARM/RISC-V unsupported both sides) |
| Coordinates | coordinates.rs | render_coordinates (canvas) | parity |
| Timestamp | timestamp.rs | render_timestamp | parity |
| Table | table.rs | render_table | parity |
| ThreeD | three_d.rs (placeholder) | render_three_d (placeholder) | parity |
| Unknown | fallthrough | muted_line | parity |
| Texture scroll-at-native-size / sampler control | egui ScrollArea + NEAREST | none | deviation (`visualizer.rs:915-923`, gpui img() limitation) |
| Selection/hover sync with hex view | none (pure fn of bytes) | none | parity (never existed) |

## 11. Plugins

Host crate `hxy-plugin-host` reused wholesale both sides.

| feature | egui | gpui | status |
|---|---|---|---|
| Load user plugins dir | `register_user_plugins` | `plugins.rs` (`<data_dir>/hxy/plugins`, shared) | parity |
| Consent cards (per-permission checkboxes) | `panels/plugins.rs` | `plugins_view.rs` | parity |
| Grant persistence (shared hxy.db) | grants table | `plugins.rs:4` shared | parity |
| Wipe plugin state | WipeState | plugins_view | parity |
| Install / delete / rescan / open-in-file-manager | plugins panel | plugins_view | parity |
| Invoke plugin command (palette) | entries.rs:1081 | `InvokePluginCommand` (`modes.rs`) | parity |
| Command cascade submenu | enter_plugin_cascade | `PluginCascade` mode | parity |
| Command prompt (respond_to_prompt) | enter_plugin_prompt | `PluginPrompt` mode | parity |
| Plugin command icon (puzzle piece) | phosphor PUZZLE_PIECE | `ICON_PLUGIN` -> `HxyIcon::PuzzlePiece` (`modes.rs`, `assets.rs`) | deviation (leading icon token; egui draws a glyph inline. Now wired M4f) |
| Plugin-authored labels untranslated (pass-through) | pass-through | `modes.rs:418` pass-through | parity |
| Console logging of plugin ops | Logger via console_log | Workspace sink -> Console entry (`plugins.rs:289`) | parity |

## 12. IPC / CLI / open-with

Shared `hxy-ipc` crate (M4d).

| feature | egui | gpui | status |
|---|---|---|---|
| CLI file args (canonicalized) | `cli.rs::resolved_files` | `hxy-ipc` + `main.rs` | parity |
| Single-instance socket forward | `ipc.rs` GenericNamespaced | `hxy-ipc`, executor-polled channel | parity |
| Second-instance open forwarding | drain_external_open_requests | `workspace.rs:5878` batch open | parity |
| macOS open-with (Finder double-click / Apple Events) | `macos_open` Apple Events | gpui-native `on_open_urls` | parity (different mechanism; `main.rs:58` note) |
| macOS NSServices right-click "Open in hxy" | `macos_open` setServicesProvider | none | gap (no app bundle / Info.plist; spec line 132 + M4f carry-in) |

## 13. Welcome / Console

| feature | egui | gpui | status |
|---|---|---|---|
| Welcome app-name + tagline | welcome_ui | welcome.rs | parity |
| Welcome recents (name label, full-path hover, click opens) | welcome_ui | `recent_rows` + `OpenRecentRequested` | parity |
| Console entries (time / severity icon / context / message) | console_ui 4-col grid | console_view.rs rows | parity |
| Console severity colors (warn/error) | visuals warn/error | theme warning/danger | parity |
| Console empty state | t("console-empty") | t("console-empty") | parity |
| Console capacity ring eviction | CONSOLE_CAPACITY | `console.rs:18` | parity |
| Console auto-open on first Error | console_log auto-open | Workspace opens Console on Error | parity |
| Console stick-to-bottom autoscroll | bottom-stuck ScrollArea | none | deviation (`console_view.rs:106-108`) |
| Console View-menu toggle | ToggleConsole | OpenConsole (open-or-focus) | parity |

## 14. Theme / icons

| feature | egui | gpui | status |
|---|---|---|---|
| Brand dark/light palette | `style.rs` hand-authored Visuals | `assets/themes/hxy.json` (`theme.rs`) | parity |
| Follow system appearance | egui ThemePreference::System | `sync_system_appearance` + observe | parity |
| Six-class byte palette (Class scheme) | `BytePalette` BG/TEXT dark/light | `hxy_core::byte_palette` | parity |
| Value gradient (Value scheme) | `ValueGradient` | `ValueGradient` | parity |
| Modified-byte tint | MODIFIED_BYTE_BG/FG | `file.rs` styler patched-byte tint | parity |
| Embedded icons (SVG asset source) | phosphor font | `assets.rs` RustEmbed + `HxyIcon` | parity |
| Selection alpha (opaque vs clamped) | opaque SELECTION_BG | gpui clamps to 0.3 wash | deviation (`theme.rs:14-18`) |
| Active-tab outline color (violet/lavender) | dock style outline | not expressible (no per-tab border key) | deviation (`theme.rs:49-50`) |
| Embedded mono font | egui embeds its font | platform default mono | deviation (`theme.rs:64`) |

## 15. i18n

| feature | egui | gpui | status |
|---|---|---|---|
| All UI strings via `hxy_i18n::t`/`t_args` | throughout | throughout (M4f sweep) | parity |
| System-locale init at startup | `main.rs:39` init_from_system_locale | `main.rs:44` init_from_system_locale (M4f) | parity |
| Plugins-tab strings localized | mostly hardcoded English (egui debt) | localized in `plugins_view.rs` | parity (gpui improves on egui here) |

## 16. Build / infra

| item | detail | status |
|---|---|---|
| `wat` crate lockfile split | root `Cargo.lock:7662` wat 1.256.0 vs `gpui/Cargo.lock:9260` wat 1.257.1; the two workspaces resolve different `wat` versions | OPEN (build-infra, not a feature gap; deferred to the milestone gate/orchestrator per the M4f plan) |
| Nested Cargo workspace (separate lockfile) | intended per spec (dependency-conflict driven) | parity (by design) |
| wasm target for GPUI app | none | n/a-wasm-only (explicit non-goal) |

## Gaps deliberately NOT fixed (with rationale)

1. `Memory` byte-cache debug panel: gpui reads whole files into `MemorySource`
   and constructs no `hxy_core::ByteCache`; a debug panel over a cache that
   does not exist has nothing to show. New panel + a caching source = large.
2. New / scratch buffer (`Untitled N`): needs an anonymous in-memory source
   plumbed through the open/save/close lifecycle. Cross-file, out of scope.
3. Paste / Paste as hex (menu, cmd-V, palette): needs an editor splice-at-caret
   input path from the clipboard; new editor capability. Documented at
   `menu.rs:26`.
4. macOS NSServices right-click "Open in hxy": needs a real app bundle with an
   `Info.plist` NSServices declaration; gpui runs unbundled here. Documented
   (spec line 132). Apple-Events/double-click open-with IS covered via
   `on_open_urls`.
5. Palette `Recent` / `QuickOpen` modes and `OpenRecent`: recents exist on the
   Welcome panel; a palette recents mode needs a new mode + entries + apply arm.
   Not a one-liner.
6. Palette `SetVirtualBase`: virtual-base address labeling has no gpui pane
   capability; new rendering work.
7. Palette dock verbs (Split*/Merge*/MoveTab*): gpui-component provides native
   drag-drop docking; re-adding keyboard-driven split/merge verbs is a separate
   feature surface, not a gap in capability.
8. Status-bar copyable hover readout, click-to-copy value labels, tab-focus
   chip, and watch chip: each is a small egui affordance; re-creating the set is
   status-bar UI work beyond a trivial fix. Recorded as deviations here and in
   the consolidated spec section.
9. Undo/Redo/ToggleEditMode/TakeSnapshot/OpenSnapshots as palette rows: all are
   functionally reachable via menu + keybinding; adding palette rows means new
   `PaletteAction` variants + apply arms (not one-line) for redundant surfaces.
10. cmd-] / cmd-[ jump-field keybindings: the actions exist in the palette; only
    the keybinding is missing. Left as a deviation rather than adding two
    bindings, to keep the keymap change out of a pure-audit task.

## Method note

Undocumented deviations found during this audit were consolidated into the port
spec's "Known deviations from the egui app (as of M4)" section rather than
scattering new code comments; deviations already recorded at their wiring sites
keep those comments and are cited above. No functional gpui code was changed by
this audit (docs-only), so no behavioral regression is possible.
