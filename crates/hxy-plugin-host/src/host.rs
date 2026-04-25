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
    /// Granted outbound-TCP allowlist patterns. Each entry is a
    /// `host:port` string with optional `*` wildcards on either
    /// side; `tcp.connect(host, port)` succeeds only when at
    /// least one pattern matches the literal host string the
    /// plugin passed (no DNS re-check). Empty list = network
    /// fully denied. Once a connection is established subsequent
    /// reads / writes don't re-check.
    pub network_allowlist: Vec<String>,
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
            network_allowlist: Vec::new(),
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

    /// Install the granted outbound-TCP allowlist. Each pattern
    /// is `host:port` with `*` wildcards permitted in either
    /// half; an empty list leaves the connection denied for all
    /// addresses.
    pub fn with_network_allowlist(mut self, patterns: Vec<String>) -> Self {
        self.network_allowlist = patterns;
        self
    }
}

/// Whether `host:port` is allowed by *any* pattern in `patterns`.
/// Pattern syntax: `<host_pattern>:<port_pattern>` where each
/// half is either a literal or `*`. Host comparison is ASCII case-
/// insensitive; port is matched after parsing the pattern half as
/// a `u16`. Malformed patterns silently never match -- callers
/// should validate at consent time, not here.
pub(crate) fn allowlist_matches(patterns: &[String], host: &str, port: u16) -> bool {
    patterns.iter().any(|p| pattern_matches(p, host, port))
}

fn pattern_matches(pattern: &str, host: &str, port: u16) -> bool {
    let Some((p_host, p_port)) = pattern.rsplit_once(':') else {
        return false;
    };
    let host_ok = p_host == "*" || p_host.eq_ignore_ascii_case(host);
    let port_ok = match p_port {
        "*" => true,
        s => s.parse::<u16>().is_ok_and(|p| p == port),
    };
    host_ok && port_ok
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
        if !allowlist_matches(&self.network_allowlist, &host, port) {
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
        StateError::InvalidName { name } => WitStateError::HostError(format!("invalid plugin name: {name:?}")),
        StateError::Backend(source) => WitStateError::HostError(format!("backend: {source}")),
    }
}

#[cfg(test)]
mod tests {
    use super::allowlist_matches;

    #[test]
    fn empty_allowlist_denies_everything() {
        assert!(!allowlist_matches(&[], "127.0.0.1", 80));
        assert!(!allowlist_matches(&[], "anywhere.example", 65535));
    }

    #[test]
    fn literal_matches_exact_host_and_port() {
        let pats = vec!["192.168.1.50:730".to_string()];
        assert!(allowlist_matches(&pats, "192.168.1.50", 730));
        assert!(!allowlist_matches(&pats, "192.168.1.51", 730));
        assert!(!allowlist_matches(&pats, "192.168.1.50", 731));
    }

    #[test]
    fn host_wildcard_matches_any_host_on_specific_port() {
        let pats = vec!["*:443".to_string()];
        assert!(allowlist_matches(&pats, "github.com", 443));
        assert!(allowlist_matches(&pats, "1.1.1.1", 443));
        assert!(!allowlist_matches(&pats, "github.com", 80));
    }

    #[test]
    fn port_wildcard_matches_any_port_on_specific_host() {
        let pats = vec!["xbox.local:*".to_string()];
        assert!(allowlist_matches(&pats, "xbox.local", 730));
        assert!(allowlist_matches(&pats, "xbox.local", 65535));
        assert!(!allowlist_matches(&pats, "other.local", 730));
    }

    #[test]
    fn full_wildcard_matches_everything() {
        let pats = vec!["*:*".to_string()];
        assert!(allowlist_matches(&pats, "anything", 0));
        assert!(allowlist_matches(&pats, "127.0.0.1", 8080));
    }

    #[test]
    fn host_match_is_case_insensitive() {
        let pats = vec!["GitHub.com:443".to_string()];
        assert!(allowlist_matches(&pats, "github.com", 443));
        assert!(allowlist_matches(&pats, "GITHUB.COM", 443));
    }

    #[test]
    fn malformed_patterns_silently_dont_match() {
        // No colon -> no match. Stops the matcher from panicking
        // on a hand-rolled / corrupted grant blob.
        let pats = vec!["nope-no-port".to_string(), "host:not-a-number".to_string()];
        assert!(!allowlist_matches(&pats, "nope-no-port", 80));
        assert!(!allowlist_matches(&pats, "host", 80));
    }

    #[test]
    fn ipv6_with_brackets_works_via_rsplit_once() {
        // rsplit_once finds the LAST `:`, so [::1]:443 splits as
        // host="[::1]" / port="443" -- IPv6 literals work as long
        // as the user / plugin author wraps them in brackets.
        let pats = vec!["[::1]:443".to_string()];
        assert!(allowlist_matches(&pats, "[::1]", 443));
    }
}

