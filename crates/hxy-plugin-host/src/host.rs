//! Host-side state and `source` / `state` import implementations. One
//! [`HostState`] lives per [`Store`](wasmtime::Store); it holds the
//! [`HexSource`] the plugin is reading from, the per-plugin name +
//! grant view used to gate `state` calls, and the [`StateStore`] that
//! actually persists blobs to disk.

use std::sync::Arc;

use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;

use crate::StateError;
use crate::StateStore;
use crate::bindings::handler_world::hxy::vfs::source::Host as SourceHost;
use crate::bindings::handler_world::hxy::vfs::state::Host as StateHost;
use crate::bindings::handler_world::hxy::vfs::state::StateError as WitStateError;

pub struct HostState {
    pub source: Arc<dyn HexSource>,
    /// Plugin identity used to namespace [`StateStore`] entries. The
    /// host provides this; the plugin can't influence which file it
    /// writes to.
    pub plugin_name: String,
    /// Whether the user granted this plugin the `persist` permission.
    /// Drives the early-return for every `state` interface call.
    pub persist_granted: bool,
    /// Shared on-disk store. `None` is treated identically to
    /// `persist_granted = false` -- the host may decide not to wire
    /// up a store at all (e.g. in tests, or when no data dir is
    /// available).
    pub state_store: Option<Arc<StateStore>>,
}

impl HostState {
    /// Construct a minimal state with no persist plumbing -- used by
    /// the existing detect / matches probe paths where the plugin
    /// shouldn't be doing I/O anyway. Calls to the `state` interface
    /// from this state always return `denied`.
    pub fn new(source: Arc<dyn HexSource>) -> Self {
        Self { source, plugin_name: String::new(), persist_granted: false, state_store: None }
    }

    /// Override the persist plumbing on an otherwise-default state.
    /// Pair with [`Self::new`] when constructing the state for a
    /// long-lived mount: the plugin's name keys the store, the grant
    /// flag gates calls, and the store handles the actual I/O.
    pub fn with_persist(mut self, plugin_name: impl Into<String>, granted: bool, store: Arc<StateStore>) -> Self {
        self.plugin_name = plugin_name.into();
        self.persist_granted = granted;
        self.state_store = Some(store);
        self
    }
}

impl SourceHost for HostState {
    fn len(&mut self) -> u64 {
        self.source.len().get()
    }

    fn read(&mut self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        let total = self.source.len().get();
        let start = offset.min(total);
        let end = offset.saturating_add(length).min(total);
        let range = ByteRange::new(ByteOffset::new(start), ByteOffset::new(end))
            .map_err(|e| format!("invalid range {start}..{end}: {e}"))?;
        self.source.read(range).map_err(|e| e.to_string())
    }
}

impl StateHost for HostState {
    fn load(&mut self) -> Result<Option<Vec<u8>>, WitStateError> {
        let (store, name) = self.persist_handle()?;
        store.load(name).map_err(map_state_error)
    }

    fn save(&mut self, blob: Vec<u8>) -> Result<(), WitStateError> {
        let (store, name) = self.persist_handle()?;
        store.save(name, &blob).map_err(map_state_error)
    }

    fn clear(&mut self) -> Result<(), WitStateError> {
        let (store, name) = self.persist_handle()?;
        store.clear(name).map_err(map_state_error)
    }
}

impl HostState {
    /// Resolve the gating for a `state` interface call. Returns
    /// `Err(WitStateError::Denied)` when persist is off or no store
    /// is wired; otherwise yields the store handle and plugin name
    /// the call should use.
    fn persist_handle(&self) -> Result<(&StateStore, &str), WitStateError> {
        if !self.persist_granted {
            return Err(WitStateError::Denied);
        }
        let Some(store) = self.state_store.as_ref() else {
            return Err(WitStateError::Denied);
        };
        if self.plugin_name.is_empty() {
            // A configured store with no plugin name is a host-side
            // wiring bug -- surface it so we don't silently write to
            // a `<empty>.bin`. Treated the same as `denied` in the
            // public surface: the plugin can't tell, and shouldn't
            // care about, which side messed up.
            return Err(WitStateError::Denied);
        }
        Ok((store.as_ref(), self.plugin_name.as_str()))
    }
}

fn map_state_error(e: StateError) -> WitStateError {
    match e {
        StateError::QuotaExceeded { limit, .. } => WitStateError::QuotaExceeded(limit),
        // Filename-policy violations are a host-side bug -- the plugin
        // doesn't pick its name -- so surfacing them as opaque
        // host-error keeps the public surface honest about who's at
        // fault.
        StateError::InvalidName { name } => WitStateError::HostError(format!("invalid plugin name: {name:?}")),
        StateError::CreateDir { path, source } => {
            WitStateError::HostError(format!("create state dir {}: {source}", path.display()))
        }
        StateError::Write { path, source } => {
            WitStateError::HostError(format!("write {}: {source}", path.display()))
        }
        StateError::Read { path, source } => {
            WitStateError::HostError(format!("read {}: {source}", path.display()))
        }
    }
}
