# M4e: GPUI Settings Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development or superpowers:executing-plans.

**Goal:** Shared settings model + SQLite persistence for the GPUI app, a native settings panel with parity to egui's, and live-apply of the settings hxy-gpui already honors (columns, input mode, minimap, offset/numeric formats, template value formats, watcher prefs, byte-cache limit, recents).

**Ordering note:** runs BEFORE M4c (plugins) because plugin grants + plugin state persistence live in the same SQLite layer.

**Reference maps (read first):** docs/superpowers/plans/2026-08-20-m4-plugins-ipc-settings-console-map.md section (3) SETTINGS; 2026-08-20-m4-gpui-architecture-map.md (add-a-panel recipe, globals, theming). Global Constraints = docs/superpowers/plans/2026-08-20-m4a-gpui-templates.md Global Constraints.

**Key decision (shared DB):** the gpui app uses the SAME `hxy.db` (settings/persist storage_dir) as the egui app: `app_settings`, `plugin_grants`, `plugin_state` are deliberately shared so both frontends see one source of truth. gpui does NOT touch egui-only keys (`window`, `open_tabs`, `dock_layout`, `vfs_tree_expanded`); its dock layout stays in gpui-dock-layout.json.

---

### Task 1: Share the settings model and persistence layer

- Create `crates/hxy-settings` (desktop-only NOT required -- model is wasm-safe, persistence is not; split: model module wasm-safe, `persist` module cfg(not(wasm32)) exactly like the egui crate does today; check how crates/hxy gates it and mirror). Move VERBATIM from crates/hxy: `settings/mod.rs` (AppSettings + all types; NumericFormat family already lives in hxy-core -- keep those imports) and `settings/persist/{mod.rs,kv.rs,plugin_state.rs,save.rs}` + the sqlx `migrations/` directory (find its path; move alongside). PersistedState itself is egui-app-specific (window/tabs): it STAYS in crates/hxy; the shared crate exposes the granular typed load_*/store_* kv API + SaveSink generalized to take the pieces (egui keeps a thin wrapper composing PersistedState from the shared calls -- keep the whole-blob save semantics by having the egui wrapper call the granular stores in one transaction if that is how save.rs works; preserve behavior exactly).
- egui refactored onto it (shims, behavior unchanged). Root gates + buckify. Commits: `refactor(hxy): extract settings model into hxy-settings`, `refactor(hxy): extract sqlite persistence into hxy-settings`, `build: ...` as needed.

### Task 2: gpui settings load/save + live-apply

- `gpui/hxy-gpui/src/settings.rs`: `SettingsGlobal(AppSettings)` + `SettingsSink` (SaveSink handle) loaded at startup (blocking pre-window like egui's load_window_settings_sync path; failures -> defaults + warn toast later). Save on mutation: gpui has no per-frame dirty poll; wrap mutations in `update_settings(cx, f)` helper that applies f, persists via SaveSink (store_app_settings), and notifies observers (cx global updated -> observers re-read).
- Live-apply parity for what gpui supports today: hex_columns (HexPane set_columns default for new panes; active panes updated), input_mode (editors set_input_mode), show_minimap/minimap_colored (HexPane flags -- check hxy-view-gpui support; add setter if the pane has minimap toggles, else record deviation), offset_base/numeric_format/template_value_formats (TemplateView/VisualizerPanel formats() reads global instead of Default -- replace the M4a placeholder), byte_value_highlight/mode/scheme (pane paint honors -- check what exists; wire what does, record the rest), address separator (geometry/format -- check), auto_reload + file_watch_prefs + poll interval (watch.rs + reload dialogs consume), byte_cache_limit (if gpui uses hxy-core byte cache -- check), recent_files (record on open; consumed by WelcomePanel -- add recents list to welcome panel mirroring egui welcome_ui), palette_escape_pops_to_parent (palette Escape cascade), zoom_factor (gpui: window-level rem scaling? check gpui-component Theme/root font scaling; record deviation if unsupported), language (DEAD in egui -- skip, note).
- Suggestion toast declines, template value formats etc. keep working. Tests: settings round-trip through the real sqlite file (tempdir override of storage_dir -- the shared crate must accept an injectable base dir for tests; check how egui tests do it), live-apply observers (columns change propagates to open panes), recents recorded+rendered.

### Task 3: gpui settings panel

- Registered dockable `SettingsPanel` (AlwaysKeep? egui closes promptless and reopens from menu; panel_kind: AlwaysKeep or dropped-on-restore? egui persists the tab via dock state: keep AlwaysKeep) opened via palette (`PaletteAction::OpenSettings`) + View menu item + keybinding parity (check egui menu for settings shortcut). Sections/rows mirroring egui settings_ui (map section 3 field list) with native widgets (Switch/Checkbox, Slider/NumberInput, Dropdown/Select, Input). Every row routes through update_settings. i18n: settings-* keys all exist.
- Tests: panel builds via registry test, a few row interactions mutate + persist (drive via reducer-level calls if widget harness is limited).

### Task 4: milestone gate

Full matrix both workspaces + buck2 + reindeer drift; boot smoke; fresh-subagent milestone review (esp. shared-DB concurrency: both apps open hxy.db WAL -- verify max_connections=1 per process and no exclusive locking regressions; settings written by gpui readable by egui and vice versa -- write a cross-read test at the shared-crate level); spec status note.
