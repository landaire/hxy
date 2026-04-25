//! Xbox 360 devkit "neighborhood" plugin.
//!
//! Surfaces a "Connect to Xbox console" command in the host's command
//! palette. Picking it prompts for the kit's `host:port`, sends an
//! XBDM NAP `WhatIsYourName` (`0x03`) packet to that address to
//! confirm the console responds, then opens a VFS-backed tab whose
//! root mirrors the console's drive list. Each drive shows up as a
//! folder; `read_dir` / `metadata` / `read_file` are translated into
//! XBDM `dirlist` / `getfileattributes` / `getfile` commands against
//! the kit.
//!
//! ## I/O bridge
//!
//! WASI preview 2 sockets gives us `std::net::TcpStream` / `UdpSocket`
//! but no async runtime. `xeedee::Client` is parameterised over
//! `futures_io::AsyncRead + AsyncWrite`, so we wrap a blocking
//! `TcpStream` in [`BlockingTcp`] (a futures shim that always
//! resolves immediately) and drive the resulting futures via
//! `futures::executor::block_on`. The single-poll completion is fine
//! because the underlying I/O really is blocking -- the future never
//! has a reason to yield `Pending`.

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use core::cell::RefCell;
use core::pin::Pin;
use core::task::Context;
use core::task::Poll;

use std::collections::HashMap;

use std::io;
use std::net::SocketAddr;
use std::net::Shutdown;
use std::net::TcpStream;
use std::net::ToSocketAddrs;
use std::net::UdpSocket;
use std::time::Duration;
use std::time::Instant;

use futures::executor::block_on;
use futures_io::AsyncRead as FAsyncRead;
use futures_io::AsyncWrite as FAsyncWrite;

use hxy_plugin_api::handler::Command;
use hxy_plugin_api::handler::FileType;
use hxy_plugin_api::handler::Guest;
use hxy_plugin_api::handler::GuestCommands;
use hxy_plugin_api::handler::GuestMount;
use hxy_plugin_api::handler::InvokeResult;
use hxy_plugin_api::handler::Metadata;
use hxy_plugin_api::handler::MountRequest;
use hxy_plugin_api::handler::PromptRequest;
use xeedee::Client;
use xeedee::Connected;
use xeedee::commands::DirEntry;
use xeedee::commands::DirList;
use xeedee::commands::DriveList;
use xeedee::discovery::DiscoveryAction;
use xeedee::discovery::{DiscoveredConsole, Discovery, DiscoveryConfig};

const PROMPT_DEFAULT: &str = "192.168.1.50:730";
const RECV_BUF: usize = 1024;

struct Plugin;

impl Guest for Plugin {
    type Mount = ConsoleMount;

    fn matches(_head: Vec<u8>) -> bool {
        // This plugin only contributes palette commands + token-driven
        // mounts. It never claims to handle a byte source -- detection
        // always falls through to other handlers.
        false
    }

    fn name() -> String {
        "xbox-neighborhood".to_string()
    }

    fn mount_source() -> Result<hxy_plugin_api::handler::exports::hxy::vfs::handler::Mount, String> {
        Err("xbox-neighborhood does not expose a file-source mount".to_string())
    }

    fn mount_by_token(
        token: String,
    ) -> Result<hxy_plugin_api::handler::exports::hxy::vfs::handler::Mount, String> {
        // Token format: `host:port` -- captured at probe time and
        // round-tripped through the host. Reconnect rather than
        // sharing state with the prior `respond` call (which lived
        // in a different Store).
        let mount = ConsoleMount::connect(&token)?;
        Ok(hxy_plugin_api::handler::exports::hxy::vfs::handler::Mount::new(mount))
    }
}

impl GuestCommands for Plugin {
    fn list_commands() -> Vec<Command> {
        vec![Command {
            id: "connect".to_string(),
            label: "Connect to Xbox console".to_string(),
            subtitle: Some("NAP unicast probe -- prompts for the kit IP".to_string()),
            icon: None,
            has_children: false,
        }]
    }

    fn invoke(id: String) -> InvokeResult {
        match id.as_str() {
            "connect" => InvokeResult::Prompt(PromptRequest {
                title: "Xbox console (host:port)".to_string(),
                default_value: Some(PROMPT_DEFAULT.to_string()),
            }),
            _ => InvokeResult::Done,
        }
    }

    fn respond(id: String, answer: String) -> InvokeResult {
        if id != "connect" {
            return InvokeResult::Done;
        }
        match probe_console(&answer) {
            Ok(console) => {
                // Mount the console as a VFS tab. The token round-
                // trips through `mount_by_token` so the new Store
                // can reconnect without sharing the probe Store's
                // state. Title carries the discovered name so the
                // tab shows "Xbox: deanxbox" instead of the bare IP.
                let token = console.addr.to_string();
                let title = format!("Xbox: {}", console.name);
                InvokeResult::Mount(MountRequest { token, title })
            }
            Err(msg) => InvokeResult::Cascade(vec![Command {
                id: "noop:probe-error".to_string(),
                label: format!("No response from {answer}"),
                subtitle: Some(msg),
                icon: None,
                has_children: false,
            }]),
        }
    }
}

// ---------------------------------------------------------------------------
// NAP unicast probe (UDP/730)
// ---------------------------------------------------------------------------

fn probe_console(host_port: &str) -> Result<DiscoveredConsole, String> {
    let target = resolve_target(host_port)?;
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind: {e}"))?;
    socket
        .set_nonblocking(true)
        .map_err(|e| format!("set_nonblocking: {e}"))?;
    let config = DiscoveryConfig::unicast(target);
    let start = Instant::now();
    let mut engine = Discovery::broadcast(config, start);
    let mut buf = [0u8; RECV_BUF];
    loop {
        match engine.poll(Instant::now()) {
            DiscoveryAction::Done(consoles) => {
                return consoles.into_iter().next().ok_or_else(|| {
                    "console did not respond within the listen window".to_string()
                });
            }
            DiscoveryAction::SendDatagram { dest, payload } => {
                socket
                    .send_to(&payload, dest)
                    .map_err(|e| format!("send_to {dest}: {e}"))?;
            }
            DiscoveryAction::Wait { until } => loop {
                let now = Instant::now();
                if now >= until {
                    break;
                }
                match socket.recv_from(&mut buf) {
                    Ok((n, src)) => {
                        engine.handle_inbound(src, &buf[..n]);
                        break;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        let remaining = until.saturating_duration_since(now);
                        let nap = remaining.min(Duration::from_millis(20));
                        std::thread::sleep(nap);
                    }
                    Err(e) => return Err(format!("recv_from: {e}")),
                }
            },
        }
    }
}

fn resolve_target(host_port: &str) -> Result<SocketAddr, String> {
    let candidate = if host_port.contains(':') {
        host_port.to_string()
    } else {
        format!("{host_port}:730")
    };
    candidate
        .as_str()
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
        .ok_or_else(|| format!("could not resolve {host_port:?}"))
}

// ---------------------------------------------------------------------------
// Console mount: drives + filesystem over XBDM TCP/730
// ---------------------------------------------------------------------------

/// One open XBDM session backing a tab. The wasm host keeps this
/// resource alive for the tab's lifetime; the inner client + transport
/// stay open across every `read_dir` / `metadata` / `read_file` call.
pub struct ConsoleMount {
    /// `RefCell` because the WIT trait methods take `&self` but we
    /// need exclusive access to drive the client's reads/writes.
    client: RefCell<Client<BlockingTcp, Connected>>,
    /// Per-path metadata (size + folder/file) cached from the most
    /// recent `dirlist` of each parent. The host renders directory
    /// entries by listing the parent then querying `metadata()` for
    /// each child -- without the cache that's `1 + N` round trips
    /// per folder; with it, just the one `dirlist`. Entries are
    /// overwritten on every `read_dir` of their parent, so a stale
    /// cache hit is at most one navigation old.
    metadata_cache: RefCell<HashMap<String, Metadata>>,
}

impl ConsoleMount {
    /// Open a TCP session to `host_port` and read the XBDM banner.
    /// Returns a connected mount ready for `read_dir` / `read_file`.
    fn connect(host_port: &str) -> Result<Self, String> {
        let stream = TcpStream::connect(host_port)
            .map_err(|e| format!("tcp connect {host_port}: {e}"))?;
        let transport = BlockingTcp::new(stream);
        let client = block_on(Client::new(transport).read_banner())
            .map_err(|e| format!("xbdm banner: {e}"))?;
        Ok(Self {
            client: RefCell::new(client),
            metadata_cache: RefCell::new(HashMap::new()),
        })
    }
}

impl GuestMount for ConsoleMount {
    fn read_dir(&self, path: String) -> Result<Vec<String>, String> {
        let kind = classify_path(&path);
        let mut client = self.client.borrow_mut();
        match kind {
            PathKind::Root => {
                // `/` -> drive list. Each drive becomes a folder
                // entry named like `E:` so the next `read_dir`
                // navigates into `/E:` and we know to dirlist `E:\`.
                let drives = block_on(client.run(DriveList))
                    .map_err(|e| format!("drivelist: {e}"))?;
                let names: Vec<String> = drives.into_iter().map(|d| format!("{d}:")).collect();
                // Pre-populate metadata so `metadata("/E:")` doesn't
                // need a round trip either.
                let mut cache = self.metadata_cache.borrow_mut();
                for name in &names {
                    cache.insert(
                        format!("/{name}"),
                        Metadata { file_type: FileType::Directory, length: 0 },
                    );
                }
                Ok(names)
            }
            PathKind::Drive(_) | PathKind::Path(_) => {
                let xbdm = path_to_xbdm(&path);
                let entries = block_on(client.run(DirList { path: xbdm.clone() }))
                    .map_err(|e| format!("dirlist {xbdm}: {e}"))?;
                // Cache size + folder/file for every entry so the
                // host's per-child `metadata()` queries don't go
                // back to the kit. `dirlist` already returns this
                // in the same response -- one round trip, N
                // metadata answers.
                let parent = path.trim_end_matches('/').to_string();
                let mut cache = self.metadata_cache.borrow_mut();
                for entry in &entries {
                    let child_path = format!("{parent}/{}", entry.name);
                    let file_type = if entry.is_directory {
                        FileType::Directory
                    } else {
                        FileType::RegularFile
                    };
                    cache.insert(child_path, Metadata { file_type, length: entry.size });
                }
                Ok(entries.into_iter().map(|e: DirEntry| e.name).collect())
            }
        }
    }

    fn metadata(&self, path: String) -> Result<Metadata, String> {
        // Synthetic root is always a directory; no XBDM call needed.
        if matches!(classify_path(&path), PathKind::Root) {
            return Ok(Metadata { file_type: FileType::Directory, length: 0 });
        }
        // Cache populated by the most recent `read_dir` of the
        // parent -- the typical browse flow visits a folder before
        // the user clicks anything inside it, so the hit rate is
        // ~100% in practice.
        if let Some(cached) = self.metadata_cache.borrow().get(&path).cloned() {
            return Ok(cached);
        }
        // Cache miss -- the host asked about a path it never
        // listed. Rather than spending a round trip on
        // `getfileattributes`, return a conservative
        // RegularFile/0-length placeholder. The user can still
        // open it (read_file works without metadata) and any
        // listing of the parent will fix the cache.
        Ok(Metadata { file_type: FileType::RegularFile, length: 0 })
    }

    fn read_file(&self, path: String) -> Result<Vec<u8>, String> {
        let kind = classify_path(&path);
        let xbdm = match kind {
            PathKind::Root | PathKind::Drive(_) => {
                return Err("not a regular file".to_string());
            }
            PathKind::Path(_) => path_to_xbdm(&path),
        };
        let mut client = self.client.borrow_mut();
        let bytes = block_on(async {
            let download = client
                .get_file(&xbdm, xeedee::commands::GetFileRange::WholeFile)
                .await?;
            download.into_vec().await
        })
        .map_err(|e| format!("getfile {xbdm}: {e}"))?;
        Ok(bytes)
    }
}

/// Coarse classification of a VFS path so each callsite doesn't have
/// to re-parse. The host always passes `/`-rooted, `/`-separated
/// paths; this maps that into the three regions of the console
/// filesystem we care about.
enum PathKind<'a> {
    /// `/` -- the synthetic root that lists drives.
    Root,
    /// `/<DRIVE>:` -- the synthetic root of a drive (a dirlist on
    /// `<DRIVE>:\` lives at the same level so the host's tree view
    /// can expand the drive without an extra hop).
    #[allow(dead_code)]
    Drive(&'a str),
    /// `/<DRIVE>:/<rest>` -- somewhere inside the filesystem.
    #[allow(dead_code)]
    Path(&'a str),
}

fn classify_path(path: &str) -> PathKind<'_> {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        return PathKind::Root;
    }
    match trimmed.split_once('/') {
        None => PathKind::Drive(trimmed),
        Some((drive, _rest)) => PathKind::Path(drive),
    }
}

/// Translate a VFS path (`/E:/Games/Halo3.xex`) into XBDM's native
/// form (`E:\Games\Halo3.xex`). Empty path components from a
/// trailing slash are dropped so `/E:/` and `/E:` both produce
/// `E:\`. The drive root always carries the trailing `\` because
/// the XBDM `dirlist` command needs it to resolve.
fn path_to_xbdm(path: &str) -> String {
    let trimmed = path.trim_start_matches('/').trim_end_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    // Special-case the drive root so a bare `/E:` becomes `E:\`
    // (XBDM needs the trailing slash on a drive listing).
    let xbdm: String = trimmed.replace('/', "\\");
    if !xbdm.contains('\\') {
        format!("{xbdm}\\")
    } else {
        xbdm
    }
}

// ---------------------------------------------------------------------------
// Blocking-TCP -> futures_io shim
// ---------------------------------------------------------------------------

/// Blocking `std::net::TcpStream` exposed as
/// `futures_io::AsyncRead + AsyncWrite`. Every poll just calls the
/// underlying blocking syscall and returns `Poll::Ready(...)`; we
/// never yield `Pending`, which is fine because `block_on` runs the
/// future on the calling thread and there's nothing else to make
/// progress. The plugin runs in its own wasi instance and the host
/// is happy to wait for the syscall.
struct BlockingTcp {
    stream: TcpStream,
}

impl BlockingTcp {
    fn new(stream: TcpStream) -> Self {
        Self { stream }
    }
}

impl FAsyncRead for BlockingTcp {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(io::Read::read(&mut self.get_mut().stream, buf))
    }
}

impl FAsyncWrite for BlockingTcp {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(io::Write::write(&mut self.get_mut().stream, buf))
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Ready(io::Write::flush(&mut self.get_mut().stream))
    }

    fn poll_close(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        // Best-effort -- wasi-sockets may not implement shutdown,
        // but dropping the stream still closes the socket.
        let _ = self.get_mut().stream.shutdown(Shutdown::Both);
        Poll::Ready(Ok(()))
    }
}

hxy_plugin_api::handler::export_handler!(Plugin with_types_in hxy_plugin_api::handler);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_classifies_as_root() {
        assert!(matches!(classify_path("/"), PathKind::Root));
        assert!(matches!(classify_path(""), PathKind::Root));
    }

    #[test]
    fn drive_classifies_as_drive() {
        assert!(matches!(classify_path("/E:"), PathKind::Drive("E:")));
    }

    #[test]
    fn path_classifies_as_path() {
        assert!(matches!(classify_path("/E:/Games"), PathKind::Path("E:")));
        assert!(matches!(classify_path("/E:/Games/Halo3.xex"), PathKind::Path("E:")));
    }

    #[test]
    fn drive_root_path_carries_trailing_backslash() {
        assert_eq!(path_to_xbdm("/E:"), "E:\\");
        assert_eq!(path_to_xbdm("/E:/"), "E:\\");
    }

    #[test]
    fn nested_path_uses_backslash_separator() {
        assert_eq!(path_to_xbdm("/E:/Games"), "E:\\Games");
        assert_eq!(path_to_xbdm("/E:/Games/Halo3.xex"), "E:\\Games\\Halo3.xex");
    }
}
