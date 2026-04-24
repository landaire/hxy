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

use crate::PluginKey;
use crate::PluginManifest;
use crate::Permissions;
use crate::bindings::handler_world::Plugin;
use crate::host::HostState;

pub struct PluginHandler {
    name: String,
    engine: Engine,
    component: Component,
    linker: Arc<Linker<HostState>>,
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
        Ok(Self { name, engine, component, linker, manifest, key, granted })
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
    pub fn granted(&self) -> Permissions {
        self.granted
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
        let mut store = Store::new(&self.engine, HostState::new(source));
        let plugin = Plugin::instantiate(&mut store, &self.component, &self.linker)
            .map_err(|e| HandlerError::Internal(format!("instantiate for mount: {e}")))?;
        let mount_resource = plugin
            .hxy_vfs_handler()
            .call_mount_source(&mut store)
            .map_err(|e| HandlerError::Internal(format!("call mount-source: {e}")))?
            .map_err(HandlerError::Malformed)?;
        let fs = Box::new(PluginFileSystem {
            inner: Mutex::new(PluginFsInner { store, plugin, mount: mount_resource }),
            plugin_name: self.name.clone(),
        });
        Ok(MountedVfs { fs, capabilities: VfsCapabilities::READ_ONLY })
    }
}

/// Live VFS backed by a plugin instance. All operations funnel through
/// the inner [`Mutex`] because wasmtime [`Store`] access requires
/// `&mut` while [`vfs::FileSystem`] methods are `&self`.
pub(crate) struct PluginFileSystem {
    pub(crate) inner: Mutex<PluginFsInner>,
    pub(crate) plugin_name: String,
}

pub(crate) struct PluginFsInner {
    pub(crate) store: Store<HostState>,
    pub(crate) plugin: Plugin,
    pub(crate) mount: ResourceAny,
}
