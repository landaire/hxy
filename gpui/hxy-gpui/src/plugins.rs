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
use gpui::component::WindowExt;
use gpui::component::notification::Notification;
use hxy_plugin_host::InvokeOutcome;
use hxy_plugin_host::MountByTokenError;
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

use crate::console::ConsoleSeverity;
use crate::panels::PluginMountIdentity;
use crate::settings::PersistHandle;
use crate::settings::PersistHandleGlobal;
use crate::workspace::Workspace;

/// The user's persisted per-plugin permission decisions. Loaded at
/// startup from the shared database and mutated through [`set_grant`].
pub struct PluginGrantsGlobal(pub PluginGrants);

impl Global for PluginGrantsGlobal {}

/// Every successfully loaded plugin handler. Rebuilt by
/// [`reload_plugins`] after a grant change or a user-requested rescan.
pub struct PluginHandlersGlobal(pub Vec<Arc<PluginHandler>>);

impl Global for PluginHandlersGlobal {}

/// Resolve a loaded handler by plugin name, or `None` when the registry
/// has no such plugin (a palette row left stale by a rescan). Cheap
/// linear scan -- the loaded set is small.
pub(crate) fn find_handler(cx: &App, name: &str) -> Option<Arc<PluginHandler>> {
    let handlers = cx.try_global::<PluginHandlersGlobal>()?;
    handlers.0.iter().find(|h| h.name() == name).cloned()
}

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

/// Activity-log sink for plugin operations: the runner reports op
/// start, completion, and failure through it. [`Workspace`] traces
/// every entry, pushes it onto the Console tab buffer, and raises a
/// toast for `Warning`/`Error`; the runner never names a concrete sink,
/// so the seam stays testable with a capturing sink.
///
/// Unlike egui's `Logger` (which owns a `VecDeque`), the gpui sink
/// takes `window`/`cx`: a toast is a window-scoped side effect, so a
/// bare `&mut self` cannot raise one.
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
                window.push_notification(Notification::warning(message.clone()), cx);
            }
            ConsoleSeverity::Error => {
                tracing::error!(context, "{message}");
                window.push_notification(Notification::error(message.clone()), cx);
            }
        }
        // Record every severity on the Console tab; an Error also
        // auto-opens it (mirrors egui `console_log`).
        self.console_log(severity, context, message, window, cx);
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
/// opaque token it round-trips, and the tab title it chose. Turned, with
/// the resolved [`MountedVfs`], into a workspace-host tab by
/// [`Workspace::finish_mount`].
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
    // Entry point wired into the palette by the `InvokePluginCommand` /
    // `RespondToPlugin` dispatch; also re-entered by `dispatch_outcome`'s
    // `Mount` arm below.
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
                    // The full `MountByTokenError` is preserved (not
                    // flattened to its message) so `finish_mount` can read
                    // `retry_label` to decide whether to offer a retry.
                    let result = cx.background_spawn(async move { worker.mount_by_token(&tok) }).await;
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
    /// egui's `dispatch_plugin_outcome`: Done closes the palette, Cascade
    /// opens a sub-menu, Prompt opens an argument-style prompt, Mount
    /// spawns the token materialization (Task 4 installs its tab).
    fn dispatch_outcome(
        &mut self,
        plugin: Arc<PluginHandler>,
        // The prompt arm answers via a `Respond` op reusing this id, so
        // the plugin can correlate the answer against its own state.
        command_id: String,
        outcome: Option<InvokeOutcome>,
        _log: &OpLog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The route is recorded inside each arm (not from `route_for`
        // before the match) so the tests prove which arm actually ran.
        match outcome {
            // Done, or a trapped / grant-denied None.
            Some(InvokeOutcome::Done) | None => {
                #[cfg(test)]
                record_test_route(cx, OutcomeRoute::Done);
                self.close_palette(window, cx);
            }
            Some(InvokeOutcome::Cascade(commands)) => {
                #[cfg(test)]
                record_test_route(cx, OutcomeRoute::Cascade);
                self.enter_plugin_cascade(plugin.name().to_owned(), commands, window, cx);
            }
            Some(InvokeOutcome::Prompt(request)) => {
                #[cfg(test)]
                record_test_route(cx, OutcomeRoute::Prompt);
                self.enter_plugin_prompt(plugin.name().to_owned(), command_id, request, window, cx);
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

    /// Apply a completed `mount-by-token`: install the workspace-host tab
    /// on success, or surface the failure. Mirrors egui's `MountReady`
    /// dispatch (`install_mount_tab` on `Ok`, a failed-mount affordance on
    /// `Err`). A recoverable failure (the plugin supplied a `retry_label`)
    /// gets an error toast with a Retry button; a structural one gets a
    /// plain error toast.
    fn finish_mount(
        &mut self,
        mount: MountRequestCtx,
        result: Result<MountedVfs, MountByTokenError>,
        log: &OpLog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(resolved) => {
                log_op_completed(self, log, true, window, cx);
                #[cfg(test)]
                record_test_mount(cx);
                let identity = PluginMountIdentity {
                    plugin_name: mount.plugin.name().to_owned(),
                    token: mount.token,
                    title: mount.title,
                };
                self.install_plugin_mount_tab(resolved, identity, window, cx);
            }
            Err(error) => match error.retry_label {
                // A recoverable failure: an error toast carrying a Retry
                // button that re-spawns the same `MountByToken` op.
                Some(_) => {
                    self.mount_failed_with_retry(mount.plugin, mount.token, mount.title, error.message, window, cx)
                }
                // A structural failure (host trap): no retry affordance,
                // since retrying without changing anything will not help.
                None => log_mount_failed(self, log, &error.message, window, cx),
            },
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
    use crate::panels::WORKSPACE_HOST_PANEL_NAME;
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
            gpui::component::init(cx);
            crate::panels::register(cx);
        });
    }

    /// A `Workspace` wrapped in a real `gpui::component::Root` (needed by
    /// `push_notification`), returned alongside its window.
    fn open_workspace(cx: &mut TestAppContext) -> (WindowHandle<gpui::component::Root>, Entity<Workspace>) {
        let window = cx.add_window(|window, cx| {
            let subscription = window.observe_window_appearance(|_, _| {});
            let workspace = cx.new(|cx| Workspace::new(Vec::new(), subscription, None, window, cx));
            gpui::component::Root::new(workspace, window, cx)
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

    /// The consent-toggle path: `set_grant` records a decision, persists
    /// it to the shared database, and the live `PluginGrantsGlobal`
    /// reflects it -- what the plugins panel's per-permission switch does
    /// on click. An independent connection confirms the write hit disk.
    #[gpui::test]
    fn set_grant_persists_and_updates_the_global(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        install_persist(cx, dir.path());

        let key = PluginKey::from_bytes("demo", "0.1.0", b"\0asm\x01\x00\x00\x00");
        let grants = PermissionGrants { persist: true, commands: true, network: vec!["host:1".into()] };

        cx.update(|cx| set_grant(cx, key.clone(), grants.clone()));

        cx.update(|cx| {
            let live = current_grants(cx);
            assert!(live.has_record(&key), "the live global records the new decision");
            assert_eq!(live.get(&key), grants);
        });

        let rt = runtime();
        let reread = rt
            .block_on(async {
                let pool = open_db_in(dir.path()).await?;
                hxy_settings::persist::load_plugin_grants(&pool).await
            })
            .expect("reload")
            .expect("grants stored");
        assert_eq!(reread.get(&key), grants, "set_grant reached the shared database");
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
        cx.update(gpui::component::init);
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

    /// A synthetic empty read-only mount, so the install / dedup paths can
    /// be driven without a live plugin (mirrors `workspace_host::empty_mount`).
    fn fake_mount() -> MountedVfs {
        MountedVfs {
            fs: Box::new(hxy_vfs::vfs::MemoryFS::new()),
            capabilities: hxy_vfs::VfsCapabilities::READ_ONLY,
            writer: None,
            virtual_base: None,
        }
    }

    fn identity(token: &str) -> PluginMountIdentity {
        PluginMountIdentity { plugin_name: "demo".into(), token: token.into(), title: format!("Mount {token}") }
    }

    fn host_tab_count(cx: &mut VisualTestContext, workspace: &Entity<Workspace>) -> usize {
        workspace.read_with(cx, |ws, cx| {
            ws.center_panel_names(cx).iter().filter(|name| name.as_str() == WORKSPACE_HOST_PANEL_NAME).count()
        })
    }

    /// Invoking the fixture's `mount` command chains through the runner
    /// (invoke -> Mount -> MountByToken) and installs a workspace-host tab
    /// bound to the plugin VFS, recorded in the mount registry.
    #[gpui::test]
    fn mount_command_opens_a_workspace_host_tab(cx: &mut TestAppContext) {
        let Some(fixture) = statecmd_fixture() else {
            eprintln!("skipping: test-statecmd fixture not built");
            return;
        };
        setup_workspace(cx);
        let (window, workspace) = open_workspace(cx);
        let plugin = load_statecmd(&fixture);
        let cx = VisualTestContext::from_window(*window, cx).into_mut();

        workspace.update_in(cx, |ws, window, cx| {
            ws.spawn_plugin_op(PluginOp::Invoke { plugin, command_id: "mount".into() }, window, cx);
        });
        cx.run_until_parked();

        assert_eq!(host_tab_count(cx, &workspace), 1, "a workspace-host tab was installed for the plugin mount");
        workspace.read_with(cx, |ws, _| assert_eq!(ws.plugin_mount_ids().len(), 1, "one live mount is registered"));
    }

    /// Installing the same `(plugin, token)` twice focuses the existing
    /// tab instead of opening a duplicate; a different token opens a
    /// second, distinct mount.
    #[gpui::test]
    fn install_dedups_same_plugin_token(cx: &mut TestAppContext) {
        setup_workspace(cx);
        let (window, workspace) = open_workspace(cx);
        let cx = VisualTestContext::from_window(*window, cx).into_mut();

        workspace.update_in(cx, |ws, window, cx| {
            ws.install_plugin_mount_tab(fake_mount(), identity("tok-a"), window, cx);
        });
        cx.run_until_parked();
        workspace.update_in(cx, |ws, window, cx| {
            ws.install_plugin_mount_tab(fake_mount(), identity("tok-a"), window, cx);
        });
        cx.run_until_parked();

        assert_eq!(host_tab_count(cx, &workspace), 1, "re-mounting the same token does not duplicate the tab");
        workspace.read_with(cx, |ws, _| assert_eq!(ws.plugin_mount_ids().len(), 1, "still a single registry record"));

        workspace.update_in(cx, |ws, window, cx| {
            ws.install_plugin_mount_tab(fake_mount(), identity("tok-b"), window, cx);
        });
        cx.run_until_parked();

        assert_eq!(host_tab_count(cx, &workspace), 2, "a different token opens a second mount tab");
        workspace.read_with(cx, |ws, _| assert_eq!(ws.plugin_mount_ids().len(), 2, "two live registry records"));
    }

    /// A recoverable mount failure surfaces a retry affordance (an error
    /// toast) and installs no tab; re-running the `MountByToken` op -- what
    /// the toast's Retry button does -- then succeeds and installs the tab.
    #[gpui::test]
    fn failed_mount_surfaces_retry_then_recovers(cx: &mut TestAppContext) {
        let Some(fixture) = statecmd_fixture() else {
            eprintln!("skipping: test-statecmd fixture not built");
            return;
        };
        setup_workspace(cx);
        let (window, workspace) = open_workspace(cx);
        let plugin = load_statecmd(&fixture);
        let cx = VisualTestContext::from_window(*window, cx).into_mut();

        let retry_plugin = plugin.clone();
        workspace.update_in(cx, |ws, window, cx| {
            let mount = MountRequestCtx { plugin: retry_plugin, token: "tok".into(), title: "Demo".into() };
            let err = MountByTokenError { message: "console offline".into(), retry_label: Some("Reconnect".into()) };
            let log = OpLog { plugin_name: "demo".into(), label: "mount tok".into(), started: Instant::now() };
            ws.finish_mount(mount, Err(err), &log, window, cx);
        });
        cx.run_until_parked();

        assert_eq!(cx.update(|window, cx| window.notifications(cx).len()), 1, "the failure surfaces a retry toast");
        assert_eq!(host_tab_count(cx, &workspace), 0, "a failed mount installs no tab");

        // The Retry button re-spawns this exact op; the fixture's
        // mount-by-token always succeeds, so the tab now installs.
        workspace.update_in(cx, |ws, window, cx| {
            let op = PluginOp::MountByToken { plugin, token: "tok".into(), title: "Demo".into() };
            ws.spawn_plugin_op(op, window, cx);
        });
        cx.run_until_parked();

        assert_eq!(host_tab_count(cx, &workspace), 1, "the retry installed the workspace-host tab");
        workspace
            .read_with(cx, |ws, _| assert_eq!(ws.plugin_mount_ids().len(), 1, "the recovered mount is registered"));
    }

    /// A structural mount failure (no retry label) routes through the
    /// `PluginConsole` seam and lands an `Error` on the Console tab
    /// buffer (published as `ConsoleLogGlobal`) carrying the plugin's own
    /// error text -- proving plugin diagnostics now reach the console.
    #[gpui::test]
    fn structural_mount_failure_lands_a_console_error(cx: &mut TestAppContext) {
        let Some(fixture) = statecmd_fixture() else {
            eprintln!("skipping: test-statecmd fixture not built");
            return;
        };
        setup_workspace(cx);
        let (window, workspace) = open_workspace(cx);
        let plugin = load_statecmd(&fixture);
        let cx = VisualTestContext::from_window(*window, cx).into_mut();

        workspace.update_in(cx, |ws, window, cx| {
            let mount = MountRequestCtx { plugin, token: "tok".into(), title: "Demo".into() };
            // No retry label -> the structural-failure path (log_mount_failed).
            let err = MountByTokenError { message: "host trap".into(), retry_label: None };
            let log = OpLog { plugin_name: "demo".into(), label: "mount tok".into(), started: Instant::now() };
            ws.finish_mount(mount, Err(err), &log, window, cx);
        });
        cx.run_until_parked();

        let entries = cx.update(|_, cx| cx.global::<crate::console::ConsoleLogGlobal>().0.clone());
        assert!(
            entries
                .iter()
                .any(|e| e.severity == crate::console::ConsoleSeverity::Error && e.message.contains("host trap")),
            "the structural mount failure is recorded as a console error",
        );
    }
}
