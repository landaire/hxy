//! Host-side state and `source` / `state` import implementations.
//!
//! One [`HostState`] lives per [`Store`](wasmtime::Store); it holds:
//!
//! - the [`HexSource`] the plugin is reading from,
//! - the per-plugin name + grant view used to gate `state` calls,
//! - the [`StateStore`] that persists blobs,
//! - a [`wasmtime::component::ResourceTable`] (used by both wasi and
//!   any future host-defined resources),
//! - a [`wasmtime_wasi::p2::WasiCtx`] providing wasi:sockets,
//!   wasi:io, wasi:cli, etc. The wasi context's socket allow-check
//!   is wired to the manifest's `network` allowlist so plugin
//!   networking inherits the same per-host:port gating as the
//!   former custom `tcp` interface.

use std::sync::Arc;

use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;
use wasmtime::component::ResourceTable;
use wasmtime_wasi::WasiCtx;
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi::WasiCtxView;
use wasmtime_wasi::WasiView;
use wasmtime_wasi::sockets::SocketAddrUse;

use crate::StateError;
use crate::StateStore;
use crate::bindings::handler_world::hxy::vfs::source::Host as SourceHost;
use crate::bindings::handler_world::hxy::vfs::state::Host as StateHost;
use crate::bindings::handler_world::hxy::vfs::state::StateError as WitStateError;

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
    /// Wasmtime resource table -- shared across host-defined and
    /// wasi-defined resources. Required by [`IoView`].
    pub resources: ResourceTable,
    /// WASI preview 2 context. Provides `wasi:sockets`,
    /// `wasi:io/streams`, `wasi:cli/environment`, etc. Built with the
    /// network allowlist callback installed when the plugin has any
    /// `network` grants; a plugin that requested no network capability
    /// gets a default (`SocketAddrCheck` denies everything) ctx.
    pub wasi: WasiCtx,
}

impl HostState {
    /// Construct a minimal state with no persist or network plumbing.
    /// Used by the existing detect / matches probe paths where the
    /// plugin shouldn't be doing I/O anyway. Calls to the `state`
    /// interface return `denied`; wasi sockets are denied by the
    /// default-deny `SocketAddrCheck`.
    pub fn new(source: Arc<dyn HexSource>) -> Self {
        Self {
            source,
            plugin_name: String::new(),
            persist_granted: false,
            state_store: None,
            resources: ResourceTable::new(),
            wasi: deny_all_wasi_ctx(),
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

    /// Install the granted outbound-network allowlist as a wasi
    /// `socket_addr_check`. Each pattern is `host:port` with `*`
    /// wildcards permitted in either half; an empty list leaves
    /// every connection denied.
    ///
    /// Note that wasi-sockets calls `socket_addr_check` with the
    /// resolved [`SocketAddr`](std::net::SocketAddr) -- the host
    /// pattern in our manifest is matched against the IP's string
    /// representation. Plugins that want by-name allowlisting (e.g.
    /// `xbox.local:*`) should declare the resolved IP they actually
    /// use, or we can wire DNS-aware allowlisting via
    /// `WasiCtxBuilder::allow_ip_name_lookup` in a follow-up.
    pub fn with_network_allowlist(mut self, patterns: Vec<String>) -> Self {
        self.wasi = build_wasi_ctx_with_allowlist(patterns);
        self
    }
}

/// Build a `WasiCtx` with sockets enabled but every address denied.
/// `socket_addr_check` defaults to "reject everything" so we don't
/// need a custom callback for the deny-all case. Stderr is
/// inherited so plugin diagnostics (`eprintln!`) reach the host's
/// terminal -- `wasi:cli/stderr` writes are otherwise dropped.
fn deny_all_wasi_ctx() -> WasiCtx {
    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stderr();
    builder.build()
}

/// Build a `WasiCtx` whose socket allow-check honors the given
/// `host:port` pattern list.
///
/// `socket_addr_check` is invoked by wasi-sockets for every socket
/// operation that involves an address: outbound `connect` / `send_to`
/// against the remote, plus local `bind` against the local address.
/// Local binds are always permitted -- the manifest's allowlist is
/// about *where the plugin can talk to*, not about whether it can
/// open a UDP socket on an ephemeral port. Outbound operations are
/// gated against the pattern list.
///
/// Stderr is inherited so plugin diagnostics (`eprintln!`) surface
/// in the host's terminal; without this they go to /dev/null and
/// debugging the plugin is much harder.
fn build_wasi_ctx_with_allowlist(patterns: Vec<String>) -> WasiCtx {
    let patterns = Arc::new(patterns);
    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stderr();
    builder.allow_ip_name_lookup(true);
    builder.socket_addr_check(move |addr, use_kind| {
        let patterns = patterns.clone();
        Box::pin(async move {
            match use_kind {
                SocketAddrUse::TcpBind | SocketAddrUse::UdpBind => true,
                SocketAddrUse::TcpConnect
                | SocketAddrUse::UdpConnect
                | SocketAddrUse::UdpOutgoingDatagram => {
                    let host = addr.ip().to_string();
                    let port = addr.port();
                    allowlist_matches(&patterns, &host, port)
                }
            }
        })
    });
    builder.build()
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

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.resources,
        }
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
        let pats = vec!["nope-no-port".to_string(), "host:not-a-number".to_string()];
        assert!(!allowlist_matches(&pats, "nope-no-port", 80));
        assert!(!allowlist_matches(&pats, "host", 80));
    }

    #[test]
    fn ipv6_with_brackets_works_via_rsplit_once() {
        let pats = vec!["[::1]:443".to_string()];
        assert!(allowlist_matches(&pats, "[::1]", 443));
    }
}
