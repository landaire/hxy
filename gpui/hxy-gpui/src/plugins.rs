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

use gpui::App;
use gpui::Global;
use hxy_plugin_host::PermissionGrants;
use hxy_plugin_host::PluginGrants;
use hxy_plugin_host::PluginHandler;
use hxy_plugin_host::PluginKey;
use hxy_plugin_host::StateStore;
use hxy_settings::persist::SqliteStateStore;
use hxy_settings::persist::load_plugin_grants;
use hxy_settings::persist::store_plugin_grants;
use hxy_vfs::VfsHandler;

use crate::settings::PersistHandle;
use crate::settings::PersistHandleGlobal;

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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::TestAppContext;
    use hxy_plugin_host::InMemoryStateStore;
    use hxy_plugin_host::PermissionGrants;
    use hxy_plugin_host::PluginGrants;
    use hxy_plugin_host::PluginKey;
    use hxy_plugin_host::StateStore;
    use hxy_settings::persist::open_db_in;
    use tokio::runtime::Runtime;

    use super::*;
    use crate::settings::PersistHandle;
    use crate::settings::PersistHandleGlobal;

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
}
