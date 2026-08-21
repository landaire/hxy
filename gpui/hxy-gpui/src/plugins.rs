//! Plugin permission grants and the loaded WASM handler registry.
//!
//! Grants persist in the shared SQLite database (the same `hxy.db`
//! the egui app writes) so a permission approved in one frontend is
//! honored by the other. [`PluginHandlersGlobal`] is the live set of
//! loaded `hxy:vfs` plugins; it is rebuilt by [`reload_plugins`]
//! whenever grants change or the user rescans.
//!
//! Every failure here degrades to a `tracing::warn`, never a panic: a
//! missing data directory, an unreadable grants blob, or a plugin
//! directory that fails to scan leaves an empty registry instead of
//! crashing the shell. The host loader aborts the whole scan on the
//! first unreadable or uncompilable plugin, so one bad plugin
//! disables the rest for that scan (same behavior as the egui
//! frontend's `register_user_plugins`).

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use gpui::App;
use gpui::Context;
use gpui::Global;
use gpui::Window;
use gpui::prelude::*;
use gpui_component::WindowExt;
use gpui_component::notification::Notification;
use hxy_plugin_host::InvokeOutcome;
use hxy_plugin_host::PermissionGrants;
use hxy_plugin_host::PluginGrants;
use hxy_plugin_host::PluginHandler;
use hxy_plugin_host::PluginKey;
use hxy_plugin_host::StateStore;
use hxy_settings::persist::SqliteStateStore;
use hxy_settings::persist::load_plugin_grants;
use hxy_settings::persist::store_plugin_grants;
use hxy_vfs::MountedVfs;
use hxy_vfs::VfsHandler;

use crate::settings::PersistHandle;
use crate::settings::PersistHandleGlobal;
use crate::workspace::Workspace;

/// The user's persisted per-plugin permission decisions. Loaded at
/// startup from the shared database and mutated through [`set_grant`].
// Field read by the consent UI (M4c Task 5) via `current_grants`.
pub struct PluginGrantsGlobal(#[allow(dead_code)] pub PluginGrants);

impl Global for PluginGrantsGlobal {}

/// Every successfully loaded plugin handler. Rebuilt by
/// [`reload_plugins`] after a grant change or a user-requested rescan.
// Field read by the op runner (M4c Task 2) and palette (Task 3).
pub struct PluginHandlersGlobal(#[allow(dead_code)] pub Vec<Arc<PluginHandler>>);

impl Global for PluginHandlersGlobal {}

/// Directory holding user-installed `hxy:vfs` plugin components
/// (`<data_dir>/hxy/plugins`). Shared with the egui app so a plugin
/// installed once is visible in both frontends. `None` when no
/// platform data directory resolves; persistence-less runs then load
/// no plugins.
pub fn user_plugins_dir() -> Option<PathBuf> {
    crate::persist::storage_dir().map(|dir| dir.join("plugins"))
}

/// Load grants and the plugin registry, installing both globals. Call
/// once at startup after [`crate::settings::init`] (it reads the
/// shared [`PersistHandle`]) and before the window opens.
pub fn init(cx: &mut App) {
    let grants = load_grants(cx);
    let store = state_store(cx);
    let handlers = build_handlers(&grants, store);
    cx.set_global(PluginGrantsGlobal(grants));
    cx.set_global(PluginHandlersGlobal(handlers));
}

/// Rebuild [`PluginHandlersGlobal`] from the current grants and the
/// on-disk plugin directory. Runs after a grant change or a rescan so
/// the linker reflects the current consent set.
// Called by the consent UI / rescan action (M4c Task 5).
#[allow(dead_code)]
pub fn reload_plugins(cx: &mut App) {
    let grants = current_grants(cx);
    let store = state_store(cx);
    let handlers = build_handlers(&grants, store);
    cx.set_global(PluginHandlersGlobal(handlers));
}

/// Record the user's decisions for `key`, persist them to the shared
/// database, and rebuild the handler registry so the new grants take
/// effect. A disk-write failure is logged, not fatal: the in-memory
/// grants still apply for this session.
// Called by the consent toggles (M4c Task 5).
#[allow(dead_code)]
pub fn set_grant(cx: &mut App, key: PluginKey, grants: PermissionGrants) {
    let mut all = current_grants(cx);
    all.set(key, grants);
    persist_grants(cx, &all);
    cx.set_global(PluginGrantsGlobal(all));
    reload_plugins(cx);
}

/// Clear a plugin's persisted state blob (the Plugins panel's "wipe
/// stored state" action). Missing store or backend error is logged,
/// not fatal.
// Called by the "wipe stored state" button (M4c Task 5).
#[allow(dead_code)]
pub fn wipe_plugin_state(cx: &App, plugin_name: &str) {
    let Some(store) = state_store(cx) else {
        tracing::warn!(plugin = plugin_name, "wipe plugin state -- no persistence store");
        return;
    };
    if let Err(err) = store.clear(plugin_name) {
        tracing::warn!(%err, plugin = plugin_name, "wipe plugin state");
    }
}

/// The live grants, or the empty baseline before [`init`] installs the
/// global (only unit-test harnesses that skip startup hit that path,
/// and empty is the correct first-boot state).
// Reached only through the grant-mutation path (M4c Task 5).
#[allow(dead_code)]
fn current_grants(cx: &App) -> PluginGrants {
    cx.try_global::<PluginGrantsGlobal>().map(|g| g.0.clone()).unwrap_or_default()
}

/// The shared persist handle, if the database opened at startup.
fn handle(cx: &App) -> Option<&PersistHandle> {
    cx.try_global::<PersistHandleGlobal>().and_then(|g| g.0.as_ref())
}

/// Build a SQLite-backed state store on the shared pool + runtime.
/// `None` when persistence is unavailable; plugins granted `persist`
/// then see their `state` calls return `denied` at runtime.
fn state_store(cx: &App) -> Option<Arc<dyn StateStore>> {
    let handle = handle(cx)?;
    Some(Arc::new(SqliteStateStore::new(handle.pool.clone(), handle.runtime.clone())))
}

/// Read the persisted grants off the shared database. A fresh
/// database (no row yet) and a missing handle both yield the empty
/// default; a decode error logs and also degrades to empty.
fn load_grants(cx: &App) -> PluginGrants {
    let Some(handle) = handle(cx) else {
        return PluginGrants::default();
    };
    match handle.runtime.block_on(load_plugin_grants(&handle.pool)) {
        Ok(Some(grants)) => grants,
        Ok(None) => PluginGrants::default(),
        Err(err) => {
            tracing::warn!(%err, "load plugin grants -- starting with none granted");
            PluginGrants::default()
        }
    }
}

/// Persist `grants` to the shared database. Logged-not-fatal on write
/// failure or when no handle exists (a persistence-less session).
// Reached through `set_grant` (M4c Task 5); exercised now by tests.
#[allow(dead_code)]
fn persist_grants(cx: &App, grants: &PluginGrants) {
    let Some(handle) = handle(cx) else {
        return;
    };
    if let Err(err) = handle.runtime.block_on(store_plugin_grants(&handle.pool, grants)) {
        tracing::warn!(%err, "persist plugin grants");
    }
}

/// Load handlers from the user plugin directory. Absent data dir ->
/// empty; a directory read error logs and yields empty.
fn build_handlers(grants: &PluginGrants, store: Option<Arc<dyn StateStore>>) -> Vec<Arc<PluginHandler>> {
    let Some(dir) = user_plugins_dir() else {
        return Vec::new();
    };
    load_handlers_in(&dir, grants, store)
}

/// Load every plugin in `dir`, wrapping each in an [`Arc`] and logging
/// its name. A nonexistent directory is not an error (the host loader
/// returns an empty list). Any other failure -- including a single
/// plugin that fails to compile, which aborts the host loader's whole
/// scan -- logs and yields empty; a reload that hits such an error
/// therefore clears the registry rather than keeping the prior set.
fn load_handlers_in(dir: &Path, grants: &PluginGrants, store: Option<Arc<dyn StateStore>>) -> Vec<Arc<PluginHandler>> {
    match hxy_plugin_host::load_plugins_from_dir(dir, grants, store) {
        Ok(handlers) => handlers
            .into_iter()
            .map(|h| {
                tracing::info!(name = h.name(), "loaded wasm plugin");
                Arc::new(h)
            })
            .collect(),
        Err(err) => {
            tracing::warn!(%err, dir = %dir.display(), "load plugins");
            Vec::new()
        }
    }
}

/// One asynchronous plugin operation the runner drives off the UI
/// thread. Each variant owns the [`Arc<PluginHandler>`] it calls, so an
/// in-flight op is independent of the registry global (a rescan can
/// swap [`PluginHandlersGlobal`] mid-flight without disturbing it).
///
/// Ports the three `PendingKind`s of the egui `plugins/runner.rs`; the
/// gpui runner needs no `rx`/`Receiver` because completion arrives
/// through an entity update rather than a per-frame channel drain.
// Constructed by the palette (M4c Task 3) and, for `MountByToken`, by
// this runner's own `Mount` dispatch.
#[allow(dead_code)]
pub enum PluginOp {
    /// Run `invoke_command(command_id)`.
    Invoke { plugin: Arc<PluginHandler>, command_id: String },
    /// Answer a prior [`InvokeOutcome::Prompt`] via
    /// `respond_to_prompt(command_id, answer)`, reusing the originating
    /// command id so the plugin can correlate the prompt.
    Respond { plugin: Arc<PluginHandler>, command_id: String, answer: String },
    /// Materialize a [`InvokeOutcome::Mount`] request via
    /// `mount_by_token(token)` -- the slow half of opening a tab.
    MountByToken { plugin: Arc<PluginHandler>, token: String, title: String },
}

impl PluginOp {
    fn plugin_name(&self) -> String {
        match self {
            PluginOp::Invoke { plugin, .. }
            | PluginOp::Respond { plugin, .. }
            | PluginOp::MountByToken { plugin, .. } => plugin.name().to_owned(),
        }
    }

    /// Short activity-log label, mirroring the egui runner's
    /// `"invoke connect"` / `"mount xbox:730"` phrasing. The command id
    /// / token is plugin-authored and stays untranslated.
    fn label(&self) -> String {
        match self {
            PluginOp::Invoke { command_id, .. } => format!("invoke {command_id}"),
            PluginOp::Respond { command_id, .. } => format!("respond {command_id}"),
            PluginOp::MountByToken { token, .. } => format!("mount {token}"),
        }
    }
}

/// Which dispatch branch a completed invoke/respond outcome routes to.
/// A pure classification of [`InvokeOutcome`] so routing is unit-
/// testable without a live workspace, and downstream milestones extend
/// one switch instead of scattering match arms.
// Read by `route_for` and the dispatch route recording; the palette /
// mount wiring that acts on each arm lands in Tasks 3 and 4.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutcomeRoute {
    /// `Done`, or a trapped / grant-denied `None`: nothing to install
    /// (Task 3 closes the palette).
    Done,
    /// `Cascade`: a sub-menu of further commands (Task 3 opens a
    /// cascade palette mode).
    Cascade,
    /// `Prompt`: the plugin needs a string (Task 3 opens a prompt
    /// palette mode that answers via `respond_to_prompt`).
    Prompt,
    /// `Mount`: materialize a token-backed VFS (this runner spawns a
    /// `MountByToken` op; Task 4 installs the resulting tab).
    Mount,
}

/// Classify an invoke/respond outcome. A trapped or grant-denied
/// `None` collapses to [`OutcomeRoute::Done`], matching egui's
/// `dispatch_plugin_outcome` where both close the palette without a
/// side effect.
#[allow(dead_code)]
pub fn route_for(outcome: &Option<InvokeOutcome>) -> OutcomeRoute {
    match outcome {
        Some(InvokeOutcome::Done) | None => OutcomeRoute::Done,
        Some(InvokeOutcome::Cascade(_)) => OutcomeRoute::Cascade,
        Some(InvokeOutcome::Prompt(_)) => OutcomeRoute::Prompt,
        Some(InvokeOutcome::Mount(_)) => OutcomeRoute::Mount,
    }
}

/// Severity of a plugin activity-log entry. Local to the gpui shell for
/// M4c; M4f folds this and the egui frontend's identical
/// `ConsoleSeverity` into one shared type when the Console tab lands.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleSeverity {
    Info,
    Warning,
    Error,
}

/// Activity-log sink for plugin operations: the runner reports op
/// start, completion, and failure through it. For M4c [`Workspace`]
/// traces every entry and raises a toast for `Warning`/`Error`; M4f
/// swaps in a Console-tab sink without touching the runner (this seam
/// is why the runner never names a concrete sink).
///
/// Unlike egui's `Logger` (which owns a `VecDeque`), the gpui sink
/// takes `window`/`cx`: a toast is a window-scoped side effect, so a
/// bare `&mut self` cannot raise one.
#[allow(dead_code)]
pub trait PluginConsole {
    fn log(&mut self, severity: ConsoleSeverity, context: String, message: String, window: &mut Window, cx: &mut App);
}

impl PluginConsole for Workspace {
    fn log(&mut self, severity: ConsoleSeverity, context: String, message: String, window: &mut Window, cx: &mut App) {
        match severity {
            // Info is a routine progress note: trace only, no toast.
            ConsoleSeverity::Info => tracing::info!(context, "{message}"),
            ConsoleSeverity::Warning => {
                tracing::warn!(context, "{message}");
                window.push_notification(Notification::warning(message), cx);
            }
            ConsoleSeverity::Error => {
                tracing::error!(context, "{message}");
                window.push_notification(Notification::error(message), cx);
            }
        }
    }
}

/// Immutable per-op logging context threaded from [`Workspace::spawn_plugin_op`]
/// through completion, so the completion handlers stay under a sane
/// argument count.
struct OpLog {
    plugin_name: String,
    label: String,
    started: Instant,
}

/// The plugin-side identity of a materialized mount: which plugin, the
/// opaque token it round-trips, and the tab title it chose. Task 4
/// turns this plus the resolved [`MountedVfs`] into a workspace-host
/// tab.
#[allow(dead_code)]
struct MountRequestCtx {
    plugin: Arc<PluginHandler>,
    token: String,
    title: String,
}

impl Workspace {
    /// Drive a plugin operation off the UI thread. The wasm call runs
    /// on the background executor (wasmtime blocks; [`PluginHandler`]
    /// is `Send + Sync`) and the outcome applies back through an entity
    /// update -- the strings-panel background pattern, one-shot per op
    /// rather than a per-frame drain.
    ///
    /// Scheduling (not running) the op here is what keeps the follow-on
    /// case sound: applying a completed outcome may call
    /// `spawn_plugin_op` again -- a `Prompt` answer whose `respond`
    /// returns `Mount` chains straight into a `MountByToken` op -- and
    /// because each call only spawns a task, there is no re-entrant
    /// borrow of the workspace entity (egui had to hand-preserve ops
    /// appended mid-drain; the entity model gets that for free).
    ///
    /// A plugin trap surfaces as the call's own `None` / `Err`, not a
    /// panic (see `PluginHandler::{invoke_command, respond_to_prompt,
    /// mount_by_token}`), so the background future always resolves and
    /// the op reaches completion rather than hanging on a "started" log.
    // Entry point wired into the palette in M4c Task 3; also re-entered
    // by `dispatch_outcome`'s `Mount` arm below.
    #[allow(dead_code)]
    pub fn spawn_plugin_op(&mut self, op: PluginOp, window: &mut Window, cx: &mut Context<Self>) {
        let log = OpLog { plugin_name: op.plugin_name(), label: op.label(), started: Instant::now() };
        log_op_started(self, &log, window, cx);
        match op {
            PluginOp::Invoke { plugin, command_id } => {
                let worker = plugin.clone();
                let id = command_id.clone();
                cx.spawn_in(window, async move |this, cx| {
                    let outcome = cx.background_spawn(async move { worker.invoke_command(&id) }).await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.finish_invoke(plugin, command_id, outcome, &log, window, cx);
                    });
                })
                .detach();
            }
            PluginOp::Respond { plugin, command_id, answer } => {
                let worker = plugin.clone();
                let id = command_id.clone();
                cx.spawn_in(window, async move |this, cx| {
                    let outcome = cx.background_spawn(async move { worker.respond_to_prompt(&id, &answer) }).await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.finish_invoke(plugin, command_id, outcome, &log, window, cx);
                    });
                })
                .detach();
            }
            PluginOp::MountByToken { plugin, token, title } => {
                let worker = plugin.clone();
                let tok = token.clone();
                cx.spawn_in(window, async move |this, cx| {
                    // The egui runner also flattened `MountByTokenError`
                    // to its message; the retry-label affordance is M4c
                    // Task 4 (retry_failed_mount).
                    let result =
                        cx.background_spawn(async move { worker.mount_by_token(&tok).map_err(|e| e.message) }).await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        let mount = MountRequestCtx { plugin, token, title };
                        this.finish_mount(mount, result, &log, window, cx);
                    });
                })
                .detach();
            }
        }
    }

    /// Apply a completed invoke/respond: log the completion, then route
    /// the outcome. Shared by both since a prompt answer fans out
    /// through the same switch as the initial activation (egui's
    /// `dispatch_plugin_outcome` is reached from both `InvokeReady` and
    /// `RespondReady`).
    fn finish_invoke(
        &mut self,
        plugin: Arc<PluginHandler>,
        command_id: String,
        outcome: Option<InvokeOutcome>,
        log: &OpLog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        log_op_completed(self, log, outcome.is_some(), window, cx);
        self.dispatch_outcome(plugin, command_id, outcome, log, window, cx);
    }

    /// Route an invoke/respond outcome to its side effect. Mirrors
    /// egui's `dispatch_plugin_outcome`; the palette (Done/Cascade/
    /// Prompt) and mount-tab (Mount) arms are downstream milestones and
    /// are left as clearly marked extension points -- deliberately no
    /// half-built palette or tab code here.
    fn dispatch_outcome(
        &mut self,
        plugin: Arc<PluginHandler>,
        // Parked for Task 3: the prompt arm answers via a `Respond` op
        // reusing this command id, and re-enters the palette from `log`.
        _command_id: String,
        outcome: Option<InvokeOutcome>,
        _log: &OpLog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The route is recorded inside each arm (not from `route_for`
        // before the match) so the tests prove which arm actually ran,
        // and the guard survives Tasks 3/4 filling the arms with behavior.
        match outcome {
            // Done, or a trapped / grant-denied None.
            Some(InvokeOutcome::Done) | None => {
                #[cfg(test)]
                record_test_route(cx, OutcomeRoute::Done);
                // Task 3: close the command palette.
            }
            Some(InvokeOutcome::Cascade(_commands)) => {
                #[cfg(test)]
                record_test_route(cx, OutcomeRoute::Cascade);
                // Task 3: open a `PluginCascade` palette mode over the
                // returned commands (egui `enter_plugin_cascade`).
            }
            Some(InvokeOutcome::Prompt(_request)) => {
                #[cfg(test)]
                record_test_route(cx, OutcomeRoute::Prompt);
                // Task 3: open a `PluginPrompt` palette mode seeded with
                // the request, answering via a `Respond` op that reuses
                // `command_id` (egui `enter_plugin_prompt`).
            }
            Some(InvokeOutcome::Mount(request)) => {
                #[cfg(test)]
                record_test_route(cx, OutcomeRoute::Mount);
                // The mount call blocks, so it runs as its own op; Task 4
                // installs the resulting tab in `finish_mount`.
                self.spawn_plugin_op(
                    PluginOp::MountByToken { plugin, token: request.token, title: request.title },
                    window,
                    cx,
                );
            }
        }
    }

    /// Apply a completed `mount-by-token`: install the tab on success
    /// (Task 4) or surface the failure. Mirrors egui's `MountReady`
    /// dispatch (`install_mount_tab` on `Ok`, `console_log(Error)` on
    /// `Err`).
    fn finish_mount(
        &mut self,
        mount: MountRequestCtx,
        result: Result<MountedVfs, String>,
        log: &OpLog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(resolved) => {
                log_op_completed(self, log, true, window, cx);
                #[cfg(test)]
                record_test_mount(cx);
                // Task 4: install `resolved` (bound to `mount.plugin`) as
                // a workspace-host tab carrying `mount.token`/`mount.title`
                // -- egui `install_mount_tab`.
                let _ = (mount, resolved);
            }
            Err(error) => {
                log_mount_failed(self, log, &error, window, cx);
            }
        }
    }
}

/// Log the start of an op. Info severity -- a routine progress note the
/// [`PluginConsole`] traces without toasting.
fn log_op_started<C: PluginConsole>(console: &mut C, log: &OpLog, window: &mut Window, cx: &mut App) {
    console.log(
        ConsoleSeverity::Info,
        format!("plugin/{}", log.plugin_name),
        format!("{} started", log.label),
        window,
        cx,
    );
}

/// Log an invoke/respond completion. `had_outcome` distinguishes a real
/// outcome (Info "ok") from a trapped / grant-denied `None` (a
/// user-facing Warning), mirroring egui's `log_plugin_completion`.
fn log_op_completed<C: PluginConsole>(
    console: &mut C,
    log: &OpLog,
    had_outcome: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let context = format!("plugin/{}", log.plugin_name);
    if had_outcome {
        // Info -> trace only; English detail is never surfaced to users.
        console.log(
            ConsoleSeverity::Info,
            context,
            format!("{} ok ({})", log.label, format_elapsed(log.started.elapsed())),
            window,
            cx,
        );
    } else {
        console.log(
            ConsoleSeverity::Warning,
            context,
            hxy_i18n::t_args("gpui-plugin-op-no-outcome", &[("plugin", &log.plugin_name)]),
            window,
            cx,
        );
    }
}

/// Log a `mount-by-token` failure. Error severity -> an error toast.
/// The plugin's own error text passes through untranslated (correct:
/// it is plugin-authored); only the surrounding chrome is localized.
fn log_mount_failed<C: PluginConsole>(console: &mut C, log: &OpLog, error: &str, window: &mut Window, cx: &mut App) {
    console.log(
        ConsoleSeverity::Error,
        format!("plugin/{}", log.plugin_name),
        hxy_i18n::t_args("gpui-plugin-mount-failed", &[("plugin", &log.plugin_name), ("error", error)]),
        window,
        cx,
    );
}

fn format_elapsed(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 { format!("{ms} ms") } else { format!("{:.2} s", d.as_secs_f64()) }
}

/// Test-only capture of which dispatch arms were reached and whether a
/// follow-on mount op ran, so the end-to-end runner tests can observe
/// routing without the palette / mount UI that Tasks 3-4 add.
#[cfg(test)]
#[derive(Default)]
struct PluginOpTestSink {
    routes: Vec<OutcomeRoute>,
    mount_attempts: usize,
}

#[cfg(test)]
impl Global for PluginOpTestSink {}

#[cfg(test)]
fn record_test_route(cx: &mut App, route: OutcomeRoute) {
    cx.default_global::<PluginOpTestSink>().routes.push(route);
}

#[cfg(test)]
fn record_test_mount(cx: &mut App) {
    cx.default_global::<PluginOpTestSink>().mount_attempts += 1;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::Entity;
    use gpui::Render;
    use gpui::TestAppContext;
    use gpui::VisualTestContext;
    use gpui::WindowHandle;
    use hxy_plugin_host::InMemoryStateStore;
    use hxy_plugin_host::MountRequest;
    use hxy_plugin_host::PermissionGrants;
    use hxy_plugin_host::PluginGrants;
    use hxy_plugin_host::PluginKey;
    use hxy_plugin_host::PromptRequest;
    use hxy_plugin_host::StateStore;
    use hxy_settings::persist::open_db_in;
    use tokio::runtime::Runtime;

    use super::*;
    use crate::settings::PersistHandle;
    use crate::settings::PersistHandleGlobal;

    /// A trivial view so a test can obtain a real `&mut Window` for the
    /// sink helpers, which take `window`/`cx` even though a capturing
    /// [`PluginConsole`] ignores them.
    struct NullView;

    impl Render for NullView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            gpui::div()
        }
    }

    /// A [`PluginConsole`] that records every entry instead of tracing /
    /// toasting, so the seam contract (severity per op result) is
    /// asserted directly.
    #[derive(Default)]
    struct CaptureConsole {
        entries: Vec<(ConsoleSeverity, String, String)>,
    }

    impl PluginConsole for CaptureConsole {
        fn log(
            &mut self,
            severity: ConsoleSeverity,
            context: String,
            message: String,
            _window: &mut Window,
            _cx: &mut App,
        ) {
            self.entries.push((severity, context, message));
        }
    }

    /// Register the panel + component globals a real [`Workspace`] needs
    /// at construction (mirrors `workspace.rs`'s test setup).
    fn setup_workspace(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::panels::register(cx);
        });
    }

    /// A `Workspace` wrapped in a real `gpui_component::Root` (needed by
    /// `push_notification`), returned alongside its window.
    fn open_workspace(cx: &mut TestAppContext) -> (WindowHandle<gpui_component::Root>, Entity<Workspace>) {
        let window = cx.add_window(|window, cx| {
            let subscription = window.observe_window_appearance(|_, _| {});
            let workspace = cx.new(|cx| Workspace::new(Vec::new(), subscription, None, window, cx));
            gpui_component::Root::new(workspace, window, cx)
        });
        let root = window.root(cx).unwrap();
        let workspace = root.read_with(cx, |root, _| root.view().clone().downcast::<Workspace>().unwrap());
        cx.run_until_parked();
        (window, workspace)
    }

    /// Stage the fixture wasm + a permissive sidecar manifest and load a
    /// single granted handler, ready to drive through the op runner.
    fn load_statecmd(fixture: &Path) -> Arc<PluginHandler> {
        let bytes = std::fs::read(fixture).expect("read fixture");
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("test-statecmd.wasm"), &bytes).expect("stage wasm");
        std::fs::write(
            dir.path().join("test-statecmd.hxy.toml"),
            "[plugin]\nname = \"test-statecmd\"\nversion = \"0.1.0\"\n\n[permissions]\npersist = true\ncommands = true\n",
        )
        .expect("stage manifest");
        let mut grants = PluginGrants::default();
        let key = PluginKey::from_bytes("test-statecmd", "0.1.0", &bytes);
        grants.set(key, PermissionGrants { persist: true, commands: true, network: vec![] });
        let store: Arc<dyn StateStore> = Arc::new(InMemoryStateStore::new());
        let handlers = load_handlers_in(dir.path(), &grants, Some(store));
        Arc::clone(handlers.first().expect("fixture handler loaded"))
    }

    fn sink_routes(cx: &mut VisualTestContext) -> Vec<OutcomeRoute> {
        cx.update(|_, cx| cx.default_global::<PluginOpTestSink>().routes.clone())
    }

    fn runtime() -> Arc<Runtime> {
        Arc::new(tokio::runtime::Builder::new_current_thread().enable_all().build().expect("build runtime"))
    }

    /// Install a real sqlite-backed persist handle rooted at `dir`,
    /// exercising the production grant persistence path.
    fn install_persist(cx: &mut TestAppContext, dir: &Path) {
        let rt = runtime();
        let pool = rt.block_on(open_db_in(dir)).expect("open db");
        cx.update(|cx| {
            cx.set_global(PersistHandleGlobal(Some(PersistHandle { pool, runtime: rt })));
            cx.set_global(PluginGrantsGlobal(PluginGrants::default()));
        });
    }

    /// The prebuilt commands+state fixture component, or `None` when a
    /// fresh checkout has not built it yet. Mirrors the hxy-plugin-host
    /// integration tests' skip-if-absent idiom.
    fn statecmd_fixture() -> Option<PathBuf> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/test-statecmd/target/wasm32-wasip2/release/hxy_plugin_test_statecmd.wasm");
        path.exists().then_some(path)
    }

    /// Grants persist through the shared handle and read back: the
    /// startup `load_grants` sees what `persist_grants` wrote, and an
    /// independent connection to the same database confirms it hit
    /// disk (proving the egui frontend would read the same value).
    #[gpui::test]
    fn grants_round_trip_through_sqlite(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        install_persist(cx, dir.path());

        let key = PluginKey::from_bytes("demo", "0.1.0", b"\0asm\x01\x00\x00\x00");
        let mut grants = PluginGrants::default();
        grants.set(key.clone(), PermissionGrants { persist: true, commands: true, network: vec![] });

        cx.update(|cx| persist_grants(cx, &grants));

        let loaded = cx.update(|cx| load_grants(cx));
        assert!(loaded.has_record(&key), "startup load sees the persisted record");
        assert_eq!(loaded.get(&key), PermissionGrants { persist: true, commands: true, network: vec![] });

        // Second, independent connection: proves the write reached the
        // shared file, not just an in-process cache.
        let rt = runtime();
        let reread = rt
            .block_on(async {
                let pool = open_db_in(dir.path()).await?;
                hxy_settings::persist::load_plugin_grants(&pool).await
            })
            .expect("reload")
            .expect("grants stored");
        assert!(reread.has_record(&key), "a second handle reads the same grant");
    }

    /// With no persist handle installed, `load_grants` yields the empty
    /// default rather than panicking.
    #[gpui::test]
    fn load_grants_without_handle_is_empty(cx: &mut TestAppContext) {
        let grants = cx.update(|cx| load_grants(cx));
        assert_eq!(grants, PluginGrants::default());
    }

    /// A nonexistent plugin directory yields an empty handler list with
    /// no panic (the host loader tolerates an absent directory).
    #[test]
    fn missing_plugins_dir_yields_no_handlers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist");
        let handlers = load_handlers_in(&missing, &PluginGrants::default(), None);
        assert!(handlers.is_empty());
    }

    /// An empty (but present) plugin directory also yields no handlers.
    #[test]
    fn empty_plugins_dir_yields_no_handlers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handlers = load_handlers_in(dir.path(), &PluginGrants::default(), None);
        assert!(handlers.is_empty());
    }

    /// The fixture plugin loads through our wrapper with its granted
    /// permissions, and its declared commands surface. Skipped when
    /// the fixture component is not built (keeps a fresh checkout's
    /// `cargo test` green). Mirrors hxy-plugin-host's staging idiom:
    /// copy the .wasm plus a sidecar manifest into a scratch dir.
    #[test]
    fn fixture_plugin_commands_surface() {
        let Some(fixture) = statecmd_fixture() else {
            eprintln!("skipping: test-statecmd fixture not built");
            return;
        };
        let bytes = std::fs::read(&fixture).expect("read fixture");

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("test-statecmd.wasm"), &bytes).expect("stage wasm");
        std::fs::write(
            dir.path().join("test-statecmd.hxy.toml"),
            "[plugin]\nname = \"test-statecmd\"\nversion = \"0.1.0\"\n\n[permissions]\npersist = true\ncommands = true\n",
        )
        .expect("stage manifest");

        // Pre-grant persist + commands, otherwise the host intersect
        // clamps them away and the commands list comes back empty.
        let mut grants = PluginGrants::default();
        let key = PluginKey::from_bytes("test-statecmd", "0.1.0", &bytes);
        grants.set(key, PermissionGrants { persist: true, commands: true, network: vec![] });

        let store: Arc<dyn StateStore> = Arc::new(InMemoryStateStore::new());
        let handlers = load_handlers_in(dir.path(), &grants, Some(store));
        let plugin = handlers.first().expect("fixture handler loaded");

        assert_eq!(plugin.manifest().expect("sidecar manifest").plugin.name, "test-statecmd");
        let cmds = plugin.list_commands();
        let ids: Vec<&str> = cmds.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["done", "cascade", "mount", "prompt", "network"]);
    }

    /// Each `InvokeOutcome` variant (and a trapped/denied `None`) maps
    /// to the dispatch arm the runner routes it to. Pure, so it covers
    /// every arm even when the fixture only exercises some.
    #[test]
    fn route_for_classifies_every_outcome() {
        assert_eq!(route_for(&Some(InvokeOutcome::Done)), OutcomeRoute::Done);
        assert_eq!(route_for(&None), OutcomeRoute::Done, "a trapped or denied None collapses to Done");
        assert_eq!(route_for(&Some(InvokeOutcome::Cascade(vec![]))), OutcomeRoute::Cascade);
        let prompt = PromptRequest { title: "Token name".into(), default_value: None };
        assert_eq!(route_for(&Some(InvokeOutcome::Prompt(prompt))), OutcomeRoute::Prompt);
        let mount = MountRequest { token: "tok".into(), title: "Tab".into() };
        assert_eq!(route_for(&Some(InvokeOutcome::Mount(mount))), OutcomeRoute::Mount);
    }

    /// The completion helpers map each op result to the right severity:
    /// a real outcome -> Info, a `None` -> Warning, a mount failure ->
    /// Error. The plugin's own error text passes through untranslated.
    #[gpui::test]
    fn completion_helpers_map_severity(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|_window, _cx| NullView);
        let cx = VisualTestContext::from_window(*window, cx).into_mut();

        let mut console = CaptureConsole::default();
        let log = OpLog { plugin_name: "demo".into(), label: "invoke connect".into(), started: Instant::now() };
        cx.update(|window, cx| {
            log_op_completed(&mut console, &log, true, window, cx);
            log_op_completed(&mut console, &log, false, window, cx);
            log_mount_failed(&mut console, &log, "connection refused", window, cx);
        });

        let severities: Vec<ConsoleSeverity> = console.entries.iter().map(|entry| entry.0).collect();
        assert_eq!(severities, vec![ConsoleSeverity::Info, ConsoleSeverity::Warning, ConsoleSeverity::Error]);
        assert!(console.entries[2].2.contains("connection refused"), "the plugin's raw error text survives");
    }

    /// Driving a real fixture command through `spawn_plugin_op` runs the
    /// wasm call off-thread and applies the outcome by entity update:
    /// invoking `done` reaches the `Done` dispatch arm.
    #[gpui::test]
    fn spawn_invoke_done_reaches_done_arm(cx: &mut TestAppContext) {
        let Some(fixture) = statecmd_fixture() else {
            eprintln!("skipping: test-statecmd fixture not built");
            return;
        };
        setup_workspace(cx);
        let (window, workspace) = open_workspace(cx);
        let plugin = load_statecmd(&fixture);
        let cx = VisualTestContext::from_window(*window, cx).into_mut();

        workspace.update_in(cx, |ws, window, cx| {
            ws.spawn_plugin_op(PluginOp::Invoke { plugin, command_id: "done".into() }, window, cx);
        });
        cx.run_until_parked();

        assert_eq!(sink_routes(cx), vec![OutcomeRoute::Done]);
    }

    /// The follow-on-op path: invoking `prompt` routes to the prompt arm;
    /// answering it with a `Respond` op returns `Mount`, which the runner
    /// chains into a `MountByToken` op that succeeds -- all through the
    /// entity-update model, with the chained op scheduled (not run inline)
    /// from inside the completed op's dispatch.
    #[gpui::test]
    fn prompt_answer_chains_into_a_mount(cx: &mut TestAppContext) {
        let Some(fixture) = statecmd_fixture() else {
            eprintln!("skipping: test-statecmd fixture not built");
            return;
        };
        setup_workspace(cx);
        let (window, workspace) = open_workspace(cx);
        let plugin = load_statecmd(&fixture);
        let cx = VisualTestContext::from_window(*window, cx).into_mut();

        let prompt_plugin = plugin.clone();
        workspace.update_in(cx, |ws, window, cx| {
            ws.spawn_plugin_op(PluginOp::Invoke { plugin: prompt_plugin, command_id: "prompt".into() }, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(sink_routes(cx), vec![OutcomeRoute::Prompt], "invoke prompt reaches the prompt arm");

        workspace.update_in(cx, |ws, window, cx| {
            let op = PluginOp::Respond { plugin, command_id: "prompt".into(), answer: "session-1".into() };
            ws.spawn_plugin_op(op, window, cx);
        });
        cx.run_until_parked();

        let (routes, mounts) = cx.update(|_, cx| {
            let sink = cx.default_global::<PluginOpTestSink>();
            (sink.routes.clone(), sink.mount_attempts)
        });
        assert_eq!(routes, vec![OutcomeRoute::Prompt, OutcomeRoute::Mount], "respond reaches the mount arm");
        assert_eq!(mounts, 1, "the chained mount-by-token op ran and succeeded");
    }
}
