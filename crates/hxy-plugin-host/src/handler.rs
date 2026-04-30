//! [`PluginHandler`] -- a loaded WASM component wrapped as a native
//! [`VfsHandler`]. Detection spins up a short-lived store; mounting
//! produces a long-lived [`PluginFileSystem`] that keeps the store and
//! the plugin's `mount` resource alive for the duration of the VFS.

use std::sync::Arc;
use std::sync::Mutex;

use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_vfs::HandlerError;
use hxy_vfs::MountedVfs;
use hxy_vfs::VfsCapabilities;
use hxy_vfs::VfsHandler;
use wasmtime::Engine;
use wasmtime::Store;
use wasmtime::component::Component;
use wasmtime::component::Linker;
use wasmtime::component::ResourceAny;

use hxy_vfs::VfsWriter;

use crate::Permissions;
use crate::PluginKey;
use crate::PluginManifest;
use crate::StateStore;
use crate::bindings::handler_world::Plugin;
use crate::commands::InvokeOutcome;
use crate::commands::PluginCommand;
use crate::host::HostState;

/// Failure returned by [`PluginHandler::mount_by_token`]. `message`
/// is plugin-supplied (or, for host-side traps, a short host-rendered
/// description). `retry_label` is `Some` when the failure is
/// recoverable by re-invoking `mount_by_token` with the same token --
/// the host renders a button with that label; `None` means no
/// affordance is shown.
#[derive(Debug, Clone)]
pub struct MountByTokenError {
    pub message: String,
    pub retry_label: Option<String>,
}

impl std::fmt::Display for MountByTokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for MountByTokenError {}

pub struct PluginHandler {
    name: String,
    engine: Engine,
    component: Component,
    linker: Arc<Linker<HostState>>,
    /// Shared persistence backend. `None` means the host didn't
    /// wire one (e.g. tests, or no data dir available); the state
    /// interface returns `denied` even for plugins that were
    /// granted `persist`.
    state_store: Option<Arc<dyn StateStore>>,
    /// Sidecar manifest, if one was found at load time. `None` is
    /// the legacy / no-permissions case -- the plugin can still
    /// mount sources (the baseline API every plugin has) but cannot
    /// use any host-provided capability that requires consent.
    manifest: Option<PluginManifest>,
    /// Stable identity = name + version + sha256(wasm). Used as the
    /// key into `PluginGrants` and `StateStore`. Populated even when
    /// no manifest was found (using the plugin's self-reported
    /// `name()` and a placeholder version of `"0.0.0"`).
    key: PluginKey,
    /// Permissions actually granted -- the manifest's request
    /// intersected with the user's stored consent. Read-only after
    /// construction; if the user toggles a grant the host should
    /// reload the plugin so the linker reflects the change.
    granted: Permissions,
}

impl PluginHandler {
    /// Instantiate once and cache the plugin's self-reported name so
    /// we don't pay for a full detection run to populate it.
    pub fn new(
        engine: Engine,
        component: Component,
        linker: Arc<Linker<HostState>>,
        state_store: Option<Arc<dyn StateStore>>,
        manifest: Option<PluginManifest>,
        key: PluginKey,
        granted: Permissions,
    ) -> Result<Self, HandlerError> {
        let placeholder: Arc<dyn HexSource> = Arc::new(MemorySource::new(Vec::new()));
        let mut store = Store::new(&engine, HostState::new(placeholder));
        let plugin = Plugin::instantiate(&mut store, &component, &linker)
            .map_err(|e| HandlerError::Internal(format!("instantiate for name probe: {e}")))?;
        let name = plugin
            .hxy_vfs_handler()
            .call_name(&mut store)
            .map_err(|e| HandlerError::Internal(format!("call name: {e}")))?;
        Ok(Self { name, engine, component, linker, state_store, manifest, key, granted })
    }

    /// Sidecar manifest, if one was found.
    pub fn manifest(&self) -> Option<&PluginManifest> {
        self.manifest.as_ref()
    }

    /// Stable identity used to key grants and persisted state.
    pub fn key(&self) -> &PluginKey {
        &self.key
    }

    /// Permissions actually granted (manifest request intersected
    /// with stored consent).
    pub fn granted(&self) -> &Permissions {
        &self.granted
    }

    /// Build the `HostState` that backs a fresh `Store` for this
    /// plugin. Pre-populates the persist plumbing whenever the
    /// plugin's persist permission is granted *and* the host wired
    /// up a state store; otherwise the plugin's `state` calls fall
    /// through to `denied` at runtime.
    fn build_host_state(&self, source: Arc<dyn HexSource>) -> HostState {
        let mut state = HostState::new(source);
        if self.granted.persist
            && let Some(store) = self.state_store.clone()
        {
            state = state.with_persist(self.key.name.clone(), true, store);
        }
        if !self.granted.network.is_empty() {
            state = state.with_network_allowlist(self.granted.network.clone());
        }
        state
    }

    /// Top-level palette commands the plugin contributes. Returns
    /// an empty list when the plugin was not granted the
    /// `commands` permission so the host can call this
    /// unconditionally without first checking the grant. The plugin
    /// is instantiated in a fresh `Store` for each call -- short-
    /// lived enough that any per-invocation state inside the plugin
    /// is by design discarded; persisted state survives via
    /// `hxy:host/state`.
    pub fn list_commands(&self) -> Vec<PluginCommand> {
        if !self.granted.commands {
            return Vec::new();
        }
        let placeholder: Arc<dyn HexSource> = Arc::new(MemorySource::new(Vec::new()));
        let mut store = Store::new(&self.engine, self.build_host_state(placeholder));
        let plugin = match Plugin::instantiate(&mut store, &self.component, &self.linker) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, plugin = %self.name, "instantiate for list-commands");
                return Vec::new();
            }
        };
        match plugin.hxy_vfs_commands().call_list_commands(&mut store) {
            Ok(list) => list.into_iter().map(PluginCommand::from_wit).collect(),
            Err(e) => {
                tracing::warn!(error = %e, plugin = %self.name, "call list-commands");
                Vec::new()
            }
        }
    }

    /// Run the plugin's `invoke` for a command id and return what
    /// the host should do next. Returns `None` when the commands
    /// permission isn't granted (the host should never have
    /// surfaced an entry the plugin couldn't be asked about) or
    /// when the underlying call traps -- both treated as "do
    /// nothing, log and move on" rather than escalating to the
    /// caller, who has no useful recovery path.
    pub fn invoke_command(&self, id: &str) -> Option<InvokeOutcome> {
        if !self.granted.commands {
            return None;
        }
        let placeholder: Arc<dyn HexSource> = Arc::new(MemorySource::new(Vec::new()));
        let mut store = Store::new(&self.engine, self.build_host_state(placeholder));
        let plugin = match Plugin::instantiate(&mut store, &self.component, &self.linker) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, plugin = %self.name, "instantiate for invoke");
                return None;
            }
        };
        match plugin.hxy_vfs_commands().call_invoke(&mut store, id) {
            Ok(result) => Some(InvokeOutcome::from_wit(result)),
            Err(e) => {
                tracing::warn!(error = %e, plugin = %self.name, command = id, "call invoke");
                None
            }
        }
    }

    /// Hand a user-supplied answer back to the plugin in response
    /// to a previous [`InvokeOutcome::Prompt`]. `id` is the same
    /// command id from the originating `invoke` call -- the plugin
    /// uses it (plus its own state) to correlate which prompt is
    /// being answered. Returns `None` under the same conditions as
    /// [`Self::invoke_command`].
    pub fn respond_to_prompt(&self, id: &str, answer: &str) -> Option<InvokeOutcome> {
        if !self.granted.commands {
            return None;
        }
        let placeholder: Arc<dyn HexSource> = Arc::new(MemorySource::new(Vec::new()));
        let mut store = Store::new(&self.engine, self.build_host_state(placeholder));
        let plugin = match Plugin::instantiate(&mut store, &self.component, &self.linker) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, plugin = %self.name, "instantiate for respond");
                return None;
            }
        };
        match plugin.hxy_vfs_commands().call_respond(&mut store, id, answer) {
            Ok(result) => Some(InvokeOutcome::from_wit(result)),
            Err(e) => {
                tracing::warn!(error = %e, plugin = %self.name, command = id, "call respond");
                None
            }
        }
    }
}

impl VfsHandler for PluginHandler {
    fn name(&self) -> &str {
        &self.name
    }

    fn matches(&self, head: &[u8]) -> bool {
        let placeholder: Arc<dyn HexSource> = Arc::new(MemorySource::new(Vec::new()));
        let mut store = Store::new(&self.engine, HostState::new(placeholder));
        let plugin = match Plugin::instantiate(&mut store, &self.component, &self.linker) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, plugin = %self.name, "instantiate for matches");
                return false;
            }
        };
        plugin.hxy_vfs_handler().call_matches(&mut store, head).unwrap_or_else(|e| {
            tracing::warn!(error = %e, plugin = %self.name, "call matches");
            false
        })
    }

    fn mount(&self, source: Arc<dyn HexSource>) -> Result<MountedVfs, HandlerError> {
        let mut store = Store::new(&self.engine, self.build_host_state(source));
        let plugin = Plugin::instantiate(&mut store, &self.component, &self.linker)
            .map_err(|e| HandlerError::Internal(format!("instantiate for mount: {e}")))?;
        let mount_resource = plugin
            .hxy_vfs_handler()
            .call_mount_source(&mut store)
            .map_err(|e| HandlerError::Internal(format!("call mount-source: {e}")))?
            .map_err(HandlerError::Malformed)?;
        let inner = Arc::new(Mutex::new(PluginFsInner { store, plugin, mount: mount_resource }));
        let block_cache =
            Arc::new(Mutex::new(crate::fs_impl::FileBlockCache::new(crate::fs_impl::FILE_BLOCK_CACHE_BUDGET_BYTES)));
        let writer: Arc<dyn VfsWriter> = Arc::new(crate::fs_impl::PluginWriter {
            inner: Arc::clone(&inner),
            block_cache: Arc::clone(&block_cache),
            plugin_name: self.name.clone(),
        });
        let meta_cache = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let fs = Box::new(PluginFileSystem {
            inner,
            plugin_name: self.name.clone(),
            dir_cache: Mutex::new(std::collections::HashMap::new()),
            meta_cache: Arc::clone(&meta_cache),
            block_cache,
        });
        Ok(MountedVfs {
            fs,
            // Plugins always claim WRITE; whether their
            // `write_range` actually does anything depends on the
            // plugin's own capability check (it'll return Err
            // for read-only paths). Marking READ_WRITE here lets
            // the editor offer the save affordance without us
            // needing a per-mount capability negotiation.
            capabilities: VfsCapabilities::READ_WRITE,
            writer: Some(writer),
            virtual_base: Some(Arc::new(PluginVirtualBaseProvider { meta_cache })),
        })
    }
}

impl PluginHandler {
    /// Mount the plugin against an opaque `token` rather than an
    /// underlying byte source. The plugin uses the token to look up
    /// which connection / profile / saved entry the user picked
    /// from a palette command; the host stays oblivious to its
    /// shape (typically a [`crate::fresh_token`]). The instantiated
    /// store has an empty placeholder source -- the plugin should
    /// not call `source.read` on a token-driven mount.
    ///
    /// Failures arrive as [`MountByTokenError`] which carries both
    /// the plugin's error message and an optional `retry_label` --
    /// when present, the host renders a button that re-invokes
    /// `mount_by_token` with the same token (xbox going offline,
    /// etc.). Host-side traps (instantiate, wasm call) carry a
    /// `None` retry-label since the failure is structural and
    /// retrying without changing anything is unlikely to help.
    pub fn mount_by_token(&self, token: &str) -> Result<MountedVfs, MountByTokenError> {
        let placeholder: Arc<dyn HexSource> = Arc::new(MemorySource::new(Vec::new()));
        let mut store = Store::new(&self.engine, self.build_host_state(placeholder));
        let plugin = Plugin::instantiate(&mut store, &self.component, &self.linker).map_err(|e| MountByTokenError {
            message: format!("instantiate for mount-by-token: {e}"),
            retry_label: None,
        })?;
        let mount_resource = plugin
            .hxy_vfs_handler()
            .call_mount_by_token(&mut store, token)
            .map_err(|e| MountByTokenError { message: format!("call mount-by-token: {e}"), retry_label: None })?
            .map_err(|e| MountByTokenError { message: e.message, retry_label: e.retry_label })?;
        let inner = Arc::new(Mutex::new(PluginFsInner { store, plugin, mount: mount_resource }));
        let block_cache =
            Arc::new(Mutex::new(crate::fs_impl::FileBlockCache::new(crate::fs_impl::FILE_BLOCK_CACHE_BUDGET_BYTES)));
        let writer: Arc<dyn VfsWriter> = Arc::new(crate::fs_impl::PluginWriter {
            inner: Arc::clone(&inner),
            block_cache: Arc::clone(&block_cache),
            plugin_name: self.name.clone(),
        });
        let meta_cache = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let fs = Box::new(PluginFileSystem {
            inner,
            plugin_name: self.name.clone(),
            dir_cache: Mutex::new(std::collections::HashMap::new()),
            meta_cache: Arc::clone(&meta_cache),
            block_cache,
        });
        Ok(MountedVfs {
            fs,
            // Plugins always claim WRITE; whether their
            // `write_range` actually does anything depends on the
            // plugin's own capability check (it'll return Err
            // for read-only paths). Marking READ_WRITE here lets
            // the editor offer the save affordance without us
            // needing a per-mount capability negotiation.
            capabilities: VfsCapabilities::READ_WRITE,
            writer: Some(writer),
            virtual_base: Some(Arc::new(PluginVirtualBaseProvider { meta_cache })),
        })
    }
}

/// Cached metadata for one path inside a plugin VFS mount. Mirrors
/// the WIT `metadata` record shape: a file/directory marker, the
/// length the host needs to size its readers against, and an
/// optional virtual base address the plugin associates with the
/// file's bytes (for load-address-bearing files like Xbox memory
/// regions). Cloned freely; held inside the per-mount `meta_cache`.
#[derive(Clone, Copy, Debug)]
pub struct MountedFileMeta {
    pub file_type: vfs::VfsFileType,
    pub len: u64,
    pub virtual_base: Option<u64>,
}

/// `VirtualBaseQuery` impl that reads from a shared `meta_cache`.
/// Constructed alongside the `PluginFileSystem` so both share the
/// same cache; a metadata fetch through the FS populates the entry
/// the query later reads. Returns `None` for paths the host hasn't
/// touched yet -- the host always queries after opening, so a miss
/// here implies the entry doesn't actually exist.
pub(crate) struct PluginVirtualBaseProvider {
    pub(crate) meta_cache: Arc<Mutex<std::collections::HashMap<String, MountedFileMeta>>>,
}

impl hxy_vfs::VirtualBaseQuery for PluginVirtualBaseProvider {
    fn virtual_base(&self, path: &str) -> Option<u64> {
        let cache = self.meta_cache.lock().ok()?;
        cache.get(path).and_then(|m| m.virtual_base)
    }
}

/// Live VFS backed by a plugin instance. All operations funnel through
/// the inner [`Mutex`] because wasmtime [`Store`] access requires
/// `&mut` while [`vfs::FileSystem`] methods are `&self`. The inner
/// is `Arc`-shared so [`crate::fs_impl::RangedReader`] (returned by
/// `open_file`) can call back into the plugin without holding a
/// reference to the surrounding `Box<dyn FileSystem>`.
///
/// `dir_cache` and `meta_cache` short-circuit repeated lookups so the
/// VFS panel's per-frame recursive walk doesn't hammer the plugin
/// (and through it, the wire) on every render. `block_cache` does
/// the same for ranged file reads -- the hex view re-reads the same
/// neighborhood every paint, so block-level LRU eliminates ~all of
/// those round trips. All caches are populated lazily on miss and
/// never expire within a session -- reload by closing + reopening
/// the tab.
pub(crate) struct PluginFileSystem {
    pub(crate) inner: Arc<Mutex<PluginFsInner>>,
    pub(crate) plugin_name: String,
    pub(crate) dir_cache: Mutex<std::collections::HashMap<String, Vec<String>>>,
    /// Cached metadata per path. `virtual_base` rides alongside the
    /// VFS-trait fields so the host can surface a plugin-supplied
    /// load address (xbox-neighborhood memory regions, in-process
    /// futures) without an extra round trip through the wasm
    /// boundary. Shared via `Arc` with [`PluginVirtualBaseProvider`]
    /// so the host can read out a path's virtual base without
    /// re-issuing a wasm call after open.
    pub(crate) meta_cache: Arc<Mutex<std::collections::HashMap<String, MountedFileMeta>>>,
    /// Byte-bounded LRU of fixed-size file blocks shared across
    /// every `RangedReader` opened against this mount. See
    /// [`crate::fs_impl::FileBlockCache`].
    pub(crate) block_cache: Arc<Mutex<crate::fs_impl::FileBlockCache>>,
}

pub(crate) struct PluginFsInner {
    pub(crate) store: Store<HostState>,
    pub(crate) plugin: Plugin,
    pub(crate) mount: ResourceAny,
}
