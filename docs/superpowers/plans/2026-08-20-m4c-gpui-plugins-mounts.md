# M4c: GPUI Plugins and Mounts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development or superpowers:executing-plans.

**Goal:** Bring the GPUI app to plugin parity: load WASM plugins with persisted permission grants, invoke plugin commands (incl. cascades and prompts) from the command palette, mount plugin-provided VFS into workspace-host tabs, and a plugins management panel (consent cards, install/rescan/delete, wipe state).

**Architecture:** `hxy-plugin-host` is already framework-agnostic (reused as-is). The egui app-glue (`plugins/runner.rs` async driver, `plugins/mount.rs` dispatch) couples to egui only via `egui::Context` repaint and temp-data queues; port it to gpui using the background executor and typed events instead. Plugin grants + plugin state persist through the shared `hxy-settings` SQLite layer (M4e) -- both apps share grants. Plugin mounts reuse the M3 workspace-host nested dock. Plugin/command diagnostics go to toasts + `tracing` for this milestone; the richer Console tab is M4f (this plan leaves a `ConsoleLog` seam so M4f can reroute without touching plugin code).

**Reference maps (read first):** docs/superpowers/plans/2026-08-20-m4-plugins-ipc-settings-console-map.md section (1) PLUGINS AND MOUNTS (the authoritative inventory: PluginHandler API, InvokeOutcome, Permissions/grants, runner.rs PendingOp/DrainResult, mount.rs dispatch_plugin_outcome/install_mount_tab, palette integration, the "no modal grant dialog -- consent is Plugins-tab checkboxes" point, the Prompt=ask-user-for-string flow). docs/superpowers/plans/2026-08-20-m4-gpui-architecture-map.md (add-a-panel recipe, workspace-host nested dock, globals, palette). Global Constraints = docs/superpowers/plans/2026-08-20-m4a-gpui-templates.md Global Constraints.

---

### Task 1: Plugin grants persistence + registry global

**Files:** gpui/hxy-gpui/src/plugins.rs (new), main.rs (load grants + plugins at startup), gpui/Cargo.toml + gpui/hxy-gpui/Cargo.toml (hxy-plugin-host, hxy-vfs deps if absent).

- `PluginGrantsGlobal(PluginGrants)` loaded from the shared SQLite (hxy-settings kv `load_plugin_grants`/`store_plugin_grants` -- verify M4e exposed these; they are in the shared kv API) at startup; `SqliteStateStore` (shared from hxy-settings persist) provides plugin state persistence.
- `PluginHandlersGlobal(Vec<Arc<PluginHandler>>)` built by porting `register_user_plugins` (map: reads user_plugins_dir() = data_dir/hxy/plugins) with the loaded grants + state store; `reload_plugins(cx)` rebuilds it (grant change, rescan). Failures -> tracing::warn, never crash.
- `set_grant(cx, key: PluginKey, grants: PermissionGrants)` -> store_plugin_grants + reload_plugins; `wipe_plugin_state(cx, name)` -> store.clear.
- Tests: grants round-trip through a tempdir SQLite (reuse the hxy-settings injectable base-dir); a fake-plugin-dir load test if a fixture wasm exists (check plugins/ for a tiny fixture; else load-empty-dir returns empty, no panic).

Commit: `feat(hxy-gpui): plugin grants persistence and handler registry`.

### Task 2: Async plugin op runner

**Files:** gpui/hxy-gpui/src/plugins.rs (extend).

- Port `plugins/runner.rs` semantics to gpui: an op enum `PluginOp::{Invoke{plugin, command_id}, Respond{plugin, command_id, answer}, MountByToken{plugin, token, title}}`; run each via `cx.background_spawn` (wasmtime calls block -- background executor is the thread pool) delivering the `InvokeOutcome`/mount `Result` back through `this.update`. No per-frame drain loop; results apply by entity update (strings.rs::run pattern). The op that spawns follow-on ops (Respond -> Mount) must enqueue correctly -- mirror the map's "push back still-pending after dispatch" subtlety with the entity-update model (a completed op's handler may start a new op).
- `ConsoleLog` seam: a `pub trait PluginLog { fn log(&mut self, severity, context, message) }`-style sink stored on Workspace; for M4c it pushes an error/warning toast and `tracing`; M4f swaps the impl. Do NOT build the console tab here.
- Tests: #[gpui::test] invoke a fake plugin command end-to-end (needs a fixture wasm exposing a command -- check plugins/test-statecmd or similar fixtures used by hxy-plugin-host tests; reuse one); assert Done/Cascade/Prompt/Mount outcomes route correctly (route table test even if some arms use a stub).

Commit: `feat(hxy-gpui): background plugin op runner`.

### Task 3: Plugin commands in the palette

**Files:** gpui/hxy-gpui/src/palette/{modes.rs,apply.rs,mod.rs}, plugins.rs.

- Port map's palette integration: main-mode entries iterate `PluginHandlersGlobal` `list_commands()` -> `PaletteAction::InvokePluginCommand{plugin_name, command_id}`; cascade sub-mode (`PaletteMode::PluginCascade` holding the returned commands) mirroring egui `enter_plugin_cascade`; prompt sub-mode (`PaletteMode::PluginPrompt`) that takes the palette input string and calls respond_to_prompt (egui `enter_plugin_prompt`). apply.rs routes InvokePluginCommand -> spawn_invoke; the cascade/prompt outcomes re-enter the palette modes. i18n: plugin command labels come from the plugin (PluginCommand.label -- not localized, plugin-authored); palette chrome strings i18n'd.
- Tests: build_entries includes plugin commands when handlers present (fake handler); cascade/prompt mode transitions unit-tested.

Commit: `feat(hxy-gpui): plugin commands in the command palette`.

### Task 4: Plugin VFS mounts into workspace-host tabs

**Files:** gpui/hxy-gpui/src/{plugins.rs,workspace.rs,panels/workspace_host.rs}.

- Port `install_mount_tab`/`dispatch_plugin_outcome` Mount arm: an `InvokeOutcome::Mount` (or a MountByToken op result) yields a `MountedVfs`; wrap it as a workspace-host tab (the M3 WorkspaceHostPanel already hosts a VFS mount + nested dock -- reuse its construction path, feeding the plugin `MountedVfs`/handler instead of an archive handler). Retry-failed-mount flow (map retry_failed_mount) as a typed action, not egui temp-data. Mounts registry so re-runs/close behave.
- Tests: mount outcome opens a workspace-host tab bound to the plugin VFS (fake VFS handler); failed mount surfaces a retry affordance + toast.

Commit: `feat(hxy-gpui): plugin VFS mounts as workspace tabs`.

### Task 5: Plugins management panel

**Files:** gpui/hxy-gpui/src/panels/plugins_view.rs (new), panels/mod.rs, persist.rs (AlwaysKeep), workspace.rs (open/singleton), menu.rs + palette (OpenPlugins), i18n ftl (NEW keys -- the egui plugins panel HARDCODES English; do NOT copy that; add plugin-* keys, root gates).

- Registered dockable `PluginsPanel` (singleton like SettingsPanel) mirroring egui `panels/plugins.rs`: consent cards (one per plugin whose manifest requests something; per-permission Switch/Checkbox for persist / commands / one-per-network-pattern; each toggle -> set_grant -> reload; "Wipe stored state" button -> wipe_plugin_state), filesystem sections (VFS handlers / template runtimes: Install via rfd, Rescan -> reload, Delete, Open-in-file-manager), and the ImHex-patterns download section (reuse M4a fetch). Emits typed events (no egui temp-data). Open via palette `OpenPlugins` + View menu item.
- i18n: full plugin-* key set (title, permissions header, per-permission labels persist/commands/network, wipe-state, install/rescan/delete/open, sections) -- these are NET NEW (egui hardcoded them).
- Tests: registry-name build, singleton open/focus, prune AlwaysKeep, a consent-toggle -> grant-stored round-trip.

Commit: `feat(hxy-gpui): plugins management panel` (+ i18n commit if large).

### Task 6: Milestone gate

Full matrix both workspaces + buck2 + reindeer + wasm check; boot smoke (app opens, plugins panel reachable, a fixture plugin's command appears in the palette if a fixture is available); fresh-subagent milestone review (grants shared with egui: a grant set by gpui readable by egui via the shared SQLite -- cross-read reasoning; the Prompt/cascade re-entrancy; mount lifecycle vs re-run; no egui temp-data patterns leaked in); spec status note M4c done.
