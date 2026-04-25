//! Host-side state and `source` / `state` / `tcp` import
//! implementations. One [`HostState`] lives per
//! [`Store`](wasmtime::Store); it holds the [`HexSource`] the plugin
//! is reading from, the per-plugin name + grant view used to gate
//! `state` and `tcp` calls, the [`StateStore`] that persists blobs,
//! and a [`wasmtime::component::ResourceTable`] for the live TCP
//! connections owned by the plugin instance.

use std::io::Read as _;
use std::io::Write as _;
use std::net::Shutdown;
use std::net::TcpStream;
use std::net::ToSocketAddrs;
use std::sync::Arc;

use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;
use wasmtime::component::Resource;
use wasmtime::component::ResourceTable;

use crate::StateError;
use crate::StateStore;
use crate::bindings::handler_world::hxy::vfs::source::Host as SourceHost;
use crate::bindings::handler_world::hxy::vfs::state::Host as StateHost;
use crate::bindings::handler_world::hxy::vfs::state::StateError as WitStateError;
use crate::bindings::handler_world::hxy::vfs::tcp::Host as TcpHost;
use crate::bindings::handler_world::hxy::vfs::tcp::HostConnection as TcpHostConnection;

pub struct HostState {
    pub source: Arc<dyn HexSource>,
    /// Plugin identity used to namespace state entries. The host
    /// provides this; the plugin can't influence which key it
    /// writes to.
    pub plugin_name: String,
    /// Whether the user granted this plugin the `persist` permission.
    /// Drives the early-return for every `state` interface call.
    pub persist_granted: bool,
    /// Shared persistence backend. `None` is treated identically to
    /// `persist_granted = false` -- the host may decide not to wire
    /// up a store at all (e.g. in tests).
    pub state_store: Option<Arc<dyn StateStore>>,
    /// Whether the user granted this plugin the `network` permission.
    /// Drives the gate for every `tcp.connect` call; once a
    /// connection exists it stays usable until dropped (no per-call
    /// re-check on `read` / `write-all`).
    pub network_granted: bool,
    /// Owns every live TCP connection the plugin opens during the
    /// lifetime of this store. wasmtime hands the plugin opaque
    /// `Resource` handles backed by entries in this table; dropping
    /// a handle from the plugin side calls back into our
    /// [`TcpHostConnection::drop`] to close the underlying socket.
    pub resources: ResourceTable,
}

impl HostState {
    /// Construct a minimal state with no persist or network plumbing
    /// -- used by the existing detect / matches probe paths where
    /// the plugin shouldn't be doing I/O anyway. Calls to the
    /// `state` interface return `denied`; calls to `tcp` return
    /// `forbidden`.
    pub fn new(source: Arc<dyn HexSource>) -> Self {
        Self {
            source,
            plugin_name: String::new(),
            persist_granted: false,
            state_store: None,
            network_granted: false,
            resources: ResourceTable::new(),
        }
    }

    /// Override the persist plumbing on an otherwise-default state.
    /// Pair with [`Self::new`] when constructing the state for a
    /// long-lived mount: the plugin's name keys the store, the grant
    /// flag gates calls, and the store handles the actual I/O.
    pub fn with_persist(
        mut self,
        plugin_name: impl Into<String>,
        granted: bool,
        store: Arc<dyn StateStore>,
    ) -> Self {
        self.plugin_name = plugin_name.into();
        self.persist_granted = granted;
        self.state_store = Some(store);
        self
    }

    /// Toggle the network grant. Granted = `tcp.connect` resolves
    /// addresses normally and returns a connection; denied =
    /// `tcp.connect` returns a `forbidden` error string with the
    /// requested host:port for diagnostics.
    pub fn with_network(mut self, granted: bool) -> Self {
        self.network_granted = granted;
        self
    }
}

/// Live TCP connection backing a plugin-side `connection` resource.
/// Wraps the std stream behind an `Option` so a plugin-issued
/// `close` can shut it down without immediately destroying the
/// resource entry; subsequent calls then return a clear error
/// instead of panicking on a moved value.
pub struct TcpConnection {
    stream: Option<TcpStream>,
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
    fn persist_handle(&self) -> Result<(&dyn StateStore, &str), WitStateError> {
        if !self.persist_granted {
            return Err(WitStateError::Denied);
        }
        let Some(store) = self.state_store.as_ref() else {
            return Err(WitStateError::Denied);
        };
        if self.plugin_name.is_empty() {
            // A configured store with no plugin name is a host-side
            // wiring bug -- surface it so we don't silently write
            // under an empty key. Treated the same as `denied` in
            // the public surface: the plugin can't tell, and
            // shouldn't care about, which side messed up.
            return Err(WitStateError::Denied);
        }
        Ok((store.as_ref(), self.plugin_name.as_str()))
    }
}

impl TcpHost for HostState {
    fn connect(&mut self, host: String, port: u16) -> Result<Resource<TcpConnection>, String> {
        if !self.network_granted {
            return Err(format!("network permission denied for {host}:{port}"));
        }
        // OS resolver. Errors propagate as-is so a plugin user
        // can distinguish "host doesn't resolve" from "denied".
        let addrs: Vec<_> = (host.as_str(), port)
            .to_socket_addrs()
            .map_err(|e| format!("resolve {host}:{port}: {e}"))?
            .collect();
        if addrs.is_empty() {
            return Err(format!("resolve {host}:{port}: no addresses"));
        }
        // Try each resolved address until one connects. The OS
        // already orders by IPv6/IPv4 preference; this just lets
        // us recover from the dual-stack case where one family is
        // unreachable.
        let mut last_err: Option<std::io::Error> = None;
        for addr in addrs {
            match TcpStream::connect(addr) {
                Ok(stream) => {
                    return self
                        .resources
                        .push(TcpConnection { stream: Some(stream) })
                        .map_err(|e| format!("push connection resource: {e}"));
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(format!("connect {host}:{port}: {}", last_err.expect("at least one error")))
    }
}

impl TcpHostConnection for HostState {
    fn write_all(&mut self, conn: Resource<TcpConnection>, data: Vec<u8>) -> Result<(), String> {
        let entry = self.resources.get_mut(&conn).map_err(|e| format!("lookup connection: {e}"))?;
        let stream = entry.stream.as_mut().ok_or_else(|| "connection closed".to_owned())?;
        stream.write_all(&data).map_err(|e| format!("write: {e}"))?;
        // Best-effort flush -- the OS usually does it on send,
        // but explicit avoids surprises for line-oriented peers.
        let _ = stream.flush();
        Ok(())
    }

    fn read(&mut self, conn: Resource<TcpConnection>, max: u32) -> Result<Vec<u8>, String> {
        let entry = self.resources.get_mut(&conn).map_err(|e| format!("lookup connection: {e}"))?;
        let stream = entry.stream.as_mut().ok_or_else(|| "connection closed".to_owned())?;
        let mut buf = vec![0u8; max as usize];
        let n = stream.read(&mut buf).map_err(|e| format!("read: {e}"))?;
        buf.truncate(n);
        Ok(buf)
    }

    fn close(&mut self, conn: Resource<TcpConnection>) {
        let Ok(entry) = self.resources.get_mut(&conn) else { return };
        if let Some(stream) = entry.stream.take() {
            // Either half failing isn't actionable here -- the
            // socket is on its way out either way.
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    fn drop(&mut self, conn: Resource<TcpConnection>) -> wasmtime::Result<()> {
        // Removing from the table drops `TcpStream`, which closes
        // the socket. Any explicit `close` already nulled the
        // stream out, so this is the catch-all path for plugins
        // that just let the resource go out of scope.
        let _ = self.resources.delete(conn)?;
        Ok(())
    }
}

fn map_state_error(e: StateError) -> WitStateError {
    match e {
        StateError::QuotaExceeded { limit, .. } => WitStateError::QuotaExceeded(limit),
        // Name-policy violations are a host-side bug -- the plugin
        // doesn't pick its name -- so surfacing them as opaque
        // host-error keeps the public surface honest about who's at
        // fault.
        StateError::InvalidName { name } => WitStateError::HostError(format!("invalid plugin name: {name:?}")),
        StateError::Backend(source) => WitStateError::HostError(format!("backend: {source}")),
    }
}
