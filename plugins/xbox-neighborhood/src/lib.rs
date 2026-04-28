//! Xbox 360 devkit "neighborhood" plugin.
//!
//! Surfaces a "Connect to Xbox console" command in the host's command
//! palette. Picking it prompts for the kit's `host:port`, sends an
//! XBDM NAP `WhatIsYourName` (`0x03`) packet to that address, and
//! (when the kit answers) opens a VFS-backed tab whose root mirrors
//! the console's drive list. Each drive shows up as a folder;
//! `read_dir` / `metadata` / `read_file` / `read_range` are routed
//! through xeedee's typed XBDM commands.
//!
//! ## I/O bridge
//!
//! WASI preview 2 sockets gives us blocking `std::net::TcpStream`
//! and `UdpSocket`. xeedee's async `Client` is parameterised over
//! `futures_io::AsyncRead + AsyncWrite`, so we wrap a blocking
//! `TcpStream` in [`BlockingTcp`] (a futures-io shim that always
//! resolves immediately) and drive the resulting futures with
//! `futures::executor::block_on`. The single-poll completion is
//! fine because the underlying I/O really is blocking -- the
//! future never has a reason to yield `Pending`.
//!
//! Reusing `Client` (rather than rolling our own state machine on
//! top of `ClientEngine`) means `Client::run` handles every typed
//! command and `Client::get_file` natively speaks the ranged
//! getfile protocol (NAME / OFFSET / SIZE plus the 4-byte little-
//! endian length prefix the kit sends after the response head).

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
use std::net::Shutdown;
use std::net::SocketAddr;
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
use xeedee::commands::GetFileRange;
use xeedee::commands::GetMem;
use xeedee::commands::ModuleInfo;
use xeedee::commands::Modules;
use xeedee::commands::SetMem;
use xeedee::commands::VirtualRegion;
use xeedee::commands::WalkMem;
use xeedee::discovery::DiscoveryAction;
use xeedee::discovery::{DiscoveredConsole, Discovery, DiscoveryConfig};

const PROMPT_DEFAULT: &str = "192.168.1.50:730";

struct Plugin;

impl Guest for Plugin {
    type Mount = ConsoleMount;

    fn matches(_head: Vec<u8>) -> bool {
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
    ) -> Result<
        hxy_plugin_api::handler::exports::hxy::vfs::handler::Mount,
        hxy_plugin_api::handler::exports::hxy::vfs::handler::MountError,
    > {
        match ConsoleMount::connect(&token) {
            Ok(mount) => Ok(hxy_plugin_api::handler::exports::hxy::vfs::handler::Mount::new(mount)),
            Err(message) => Err(hxy_plugin_api::handler::exports::hxy::vfs::handler::MountError {
                message,
                // Connection failures are recoverable: the user can
                // power the kit on, plug in the network cable, etc.
                // and click the host's retry button to call this
                // again with the same address token.
                retry_label: Some("Reconnect to xbox".to_string()),
            }),
        }
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

fn probe_console(host_port: &str) -> Result<DiscoveredConsole, String> {
    let target = resolve_target(host_port)?;
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind: {e}"))?;
    socket
        .set_nonblocking(true)
        .map_err(|e| format!("set_nonblocking: {e}"))?;
    let config = DiscoveryConfig::unicast(target);
    let start = Instant::now();
    let mut engine = Discovery::broadcast(config, start);
    let mut buf = [0u8; 1024];
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

/// One open XBDM session backing a tab. Holds an async `Client`
/// driven by `block_on` over a blocking TCP stream. The wasm host
/// keeps this resource alive for the tab's lifetime; the connection
/// stays open across every `read_dir` / `metadata` / `read_file` /
/// `read_range` call.
pub struct ConsoleMount {
    client: RefCell<Client<BlockingTcp, Connected>>,
    /// Cached `Metadata` per VFS path, populated from the most
    /// recent `dirlist` / module listing / memory walk so per-entry
    /// `metadata()` queries don't go back to the kit. The host's
    /// `PluginFileSystem::meta_cache` provides another layer in
    /// front of this, but keeping a per-mount cache means even an
    /// uncached call avoids a full XBDM round trip.
    metadata_cache: RefCell<HashMap<String, Metadata>>,
    /// Lazy-loaded `modules` snapshot. Reading a `/modules/<name>`
    /// entry needs the module's base address; we look it up here.
    /// Listed once per session; the user can close + reopen the
    /// tab if a new module loaded.
    modules: RefCell<Option<HashMap<String, ModuleInfo>>>,
    /// Lazy-loaded `walkmem` snapshot keyed by hex base address
    /// (`"80000000"`). Same lifetime story as `modules`.
    memory_regions: RefCell<Option<HashMap<String, VirtualRegion>>>,
}

impl ConsoleMount {
    /// Open a TCP session to `host_port`, read the XBDM banner,
    /// and return a connected mount.
    fn connect(host_port: &str) -> Result<Self, String> {
        let stream = TcpStream::connect(host_port)
            .map_err(|e| format!("tcp connect {host_port}: {e}"))?;
        let transport = BlockingTcp::new(stream);
        let client = block_on(Client::new(transport).read_banner())
            .map_err(|e| format!("xbdm banner: {e}"))?;
        Ok(Self {
            client: RefCell::new(client),
            metadata_cache: RefCell::new(HashMap::new()),
            modules: RefCell::new(None),
            memory_regions: RefCell::new(None),
        })
    }

    /// Lazy-fetch the module list. Cached for the mount's lifetime;
    /// close + reopen the tab to refresh.
    fn load_modules(&self) -> Result<(), String> {
        if self.modules.borrow().is_some() {
            return Ok(());
        }
        let mut client = self.client.borrow_mut();
        let infos = block_on(client.run(Modules)).map_err(|e| format!("modules: {e}"))?;
        let mut by_name = HashMap::with_capacity(infos.len());
        let mut cache = self.metadata_cache.borrow_mut();
        for info in infos {
            let name = info.name.clone();
            cache.insert(
                format!("/memory/modules/{name}"),
                Metadata { file_type: FileType::RegularFile, length: info.size as u64 },
            );
            by_name.insert(name, info);
        }
        *self.modules.borrow_mut() = Some(by_name);
        Ok(())
    }

    /// Lazy-fetch the virtual-memory region list (`walkmem`). Each
    /// entry is named by its hex base address so
    /// `/memory/maps/80000000` resolves cleanly.
    fn load_memory_regions(&self) -> Result<(), String> {
        if self.memory_regions.borrow().is_some() {
            return Ok(());
        }
        let mut client = self.client.borrow_mut();
        let regions = block_on(client.run(WalkMem)).map_err(|e| format!("walkmem: {e}"))?;
        let mut by_name = HashMap::with_capacity(regions.len());
        let mut cache = self.metadata_cache.borrow_mut();
        for region in regions {
            let name = format!("{:08x}", region.base);
            cache.insert(
                format!("/memory/maps/{name}"),
                Metadata { file_type: FileType::RegularFile, length: region.size as u64 },
            );
            by_name.insert(name, region);
        }
        *self.memory_regions.borrow_mut() = Some(by_name);
        Ok(())
    }
}

/// VFS path classification including the synthetic namespaces.
///
/// Top-level layout:
///
///   /physical/<DRIVE>:/...        xbdm filesystem (dirlist + getfile)
///   /memory/maps/<hex_base>       walkmem region (getmem + setmem)
///   /memory/modules/<name>        loaded module image (getmem + setmem)
///
/// The two `*Root` variants mark the synthetic dir endpoints
/// (`read_dir` lists their children, `metadata` returns Directory).
enum SyntheticKind<'a> {
    /// `/memory/modules/<name>` -- read/write backed by
    /// `getmem`/`setmem` against the module's loaded image.
    Module(&'a str),
    /// `/memory/maps/<hex_base>` -- read/write backed by
    /// `getmem`/`setmem` against a walkmem-discovered region.
    Memory(&'a str),
}

fn classify_synthetic(path: &str) -> Option<SyntheticKind<'_>> {
    let trimmed = path.trim_start_matches('/');
    if let Some(rest) = trimmed.strip_prefix("memory/modules/")
        && !rest.is_empty()
    {
        return Some(SyntheticKind::Module(rest));
    }
    if let Some(rest) = trimmed.strip_prefix("memory/maps/")
        && !rest.is_empty()
    {
        return Some(SyntheticKind::Memory(rest));
    }
    None
}

impl GuestMount for ConsoleMount {
    fn read_dir(&self, path: String) -> Result<Vec<String>, String> {
        let kind = classify_path(&path);
        match kind {
            PathKind::Root => {
                // Top-level: just the two synthetic categories.
                // Their child listings are deferred until the user
                // expands them.
                let mut cache = self.metadata_cache.borrow_mut();
                for synthetic in ["physical", "memory"] {
                    cache.insert(
                        format!("/{synthetic}"),
                        Metadata { file_type: FileType::Directory, length: 0 },
                    );
                }
                Ok(vec!["physical".to_string(), "memory".to_string()])
            }
            PathKind::PhysicalRoot => {
                // Drive list -- one xbdm `drivelist` round trip.
                let mut client = self.client.borrow_mut();
                let drives = block_on(client.run(DriveList))
                    .map_err(|e| format!("drivelist: {e}"))?;
                let names: Vec<String> = drives.into_iter().map(|d| format!("{d}:")).collect();
                let mut cache = self.metadata_cache.borrow_mut();
                for name in &names {
                    cache.insert(
                        format!("/physical/{name}"),
                        Metadata { file_type: FileType::Directory, length: 0 },
                    );
                }
                Ok(names)
            }
            PathKind::MemoryRoot => {
                let mut cache = self.metadata_cache.borrow_mut();
                for child in ["maps", "modules"] {
                    cache.insert(
                        format!("/memory/{child}"),
                        Metadata { file_type: FileType::Directory, length: 0 },
                    );
                }
                Ok(vec!["maps".to_string(), "modules".to_string()])
            }
            PathKind::MemoryCategory => {
                let trimmed = path.trim_start_matches('/').trim_end_matches('/');
                if trimmed == "memory/maps" {
                    self.list_memory_regions()
                } else {
                    // memory/modules
                    self.list_modules()
                }
            }
            PathKind::Drive(_) | PathKind::Path(_) => {
                let xbdm = path_to_xbdm(&path);
                let mut client = self.client.borrow_mut();
                let entries = block_on(client.run(DirList { path: xbdm.clone() }))
                    .map_err(|e| format!("dirlist {xbdm}: {e}"))?;
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
            PathKind::MemoryEntry => Err(format!("not a directory: {path}")),
            PathKind::Unknown => Err(format!("unknown path: {path}")),
        }
    }

    fn metadata(&self, path: String) -> Result<Metadata, String> {
        match classify_path(&path) {
            PathKind::Root
            | PathKind::PhysicalRoot
            | PathKind::MemoryRoot
            | PathKind::MemoryCategory => {
                return Ok(Metadata { file_type: FileType::Directory, length: 0 });
            }
            _ => {}
        }
        if let Some(cached) = self.metadata_cache.borrow().get(&path).cloned() {
            return Ok(cached);
        }
        // Cache miss on a synthetic path: lazy-load the matching
        // category so length is known. Without this, session restore
        // (which goes straight to `read_file` without first listing
        // the parent directory) would see length=0 and open an empty
        // tab instead of the real bytes.
        if let Some(kind) = classify_synthetic(&path) {
            match kind {
                SyntheticKind::Module(_) => {
                    self.load_modules()?;
                }
                SyntheticKind::Memory(_) => {
                    self.load_memory_regions()?;
                }
            }
            if let Some(cached) = self.metadata_cache.borrow().get(&path).cloned() {
                return Ok(cached);
            }
        }
        // Cache miss on a non-synthetic path the host never listed.
        // Return a placeholder rather than spending a round trip; the
        // user can still open the file and any listing of the parent
        // will fix the cache.
        Ok(Metadata { file_type: FileType::RegularFile, length: 0 })
    }

    fn read_file(&self, path: String) -> Result<Vec<u8>, String> {
        let length = self.metadata(path.clone())?.length;
        if length == 0 {
            return Ok(Vec::new());
        }
        self.read_range(path, 0, length)
    }

    fn read_range(&self, path: String, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        if length == 0 {
            return Ok(Vec::new());
        }
        // Synthetic memory-backed reads (`/memory/maps/...`,
        // `/memory/modules/...`) translate to xbdm `getmem` against
        // a looked-up base address.
        if let Some(synth) = classify_synthetic(&path) {
            return self.read_synthetic(synth, offset, length);
        }
        match classify_path(&path) {
            PathKind::Path(_) => {
                let xbdm = path_to_xbdm(&path);
                let mut client = self.client.borrow_mut();
                let bytes = block_on(async {
                    let download = client
                        .get_file(&xbdm, GetFileRange::Range { offset, size: length })
                        .await?;
                    download.into_vec().await
                })
                .map_err(|e| format!("getfile {xbdm} @{offset}+{length}: {e}"))?;
                Ok(bytes)
            }
            _ => Err(format!("not a regular file: {path}")),
        }
    }

    fn write_range(&self, path: String, offset: u64, data: Vec<u8>) -> Result<u64, String> {
        if data.is_empty() {
            return Ok(0);
        }
        // Only the synthetic memory namespaces are pokeable today.
        // Drive files (`/physical/E:/...`) would need `sendfile` /
        // `writefile`; deferred until there's a real use case.
        let Some(synth) = classify_synthetic(&path) else {
            return Err(format!(
                "write_range only supports /memory/maps/ and /memory/modules/ paths, got {path:?}"
            ));
        };
        self.write_synthetic(synth, offset, data)
    }
}

impl ConsoleMount {
    fn list_modules(&self) -> Result<Vec<String>, String> {
        self.load_modules()?;
        let modules = self.modules.borrow();
        let map = modules.as_ref().expect("loaded above");
        let mut names: Vec<String> = map.keys().cloned().collect();
        names.sort();
        Ok(names)
    }

    fn list_memory_regions(&self) -> Result<Vec<String>, String> {
        self.load_memory_regions()?;
        let regions = self.memory_regions.borrow();
        let map = regions.as_ref().expect("loaded above");
        let mut names: Vec<String> = map.keys().cloned().collect();
        // Sort lexicographically -- since names are zero-padded
        // 8-char hex, that doubles as numeric order.
        names.sort();
        Ok(names)
    }

    /// Resolve a synthetic memory-backed path to an absolute
    /// virtual address + bound, then pull `length` bytes via
    /// `getmem`. Reads beyond the entry's declared size clamp.
    fn read_synthetic(
        &self,
        kind: SyntheticKind<'_>,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, String> {
        let (base, total_size) = match kind {
            SyntheticKind::Module(name) => {
                self.load_modules()?;
                let modules = self.modules.borrow();
                let info = modules
                    .as_ref()
                    .expect("loaded")
                    .get(name)
                    .ok_or_else(|| format!("unknown module {name:?}"))?;
                (info.base, info.size as u64)
            }
            SyntheticKind::Memory(hex) => {
                self.load_memory_regions()?;
                let regions = self.memory_regions.borrow();
                let region = regions
                    .as_ref()
                    .expect("loaded")
                    .get(hex)
                    .ok_or_else(|| format!("unknown memory region {hex:?}"))?;
                (region.base, region.size as u64)
            }
        };
        let end = offset.saturating_add(length).min(total_size);
        let want = end.saturating_sub(offset);
        if want == 0 {
            return Ok(Vec::new());
        }
        // `GetMem.length` is `u32`. Each call already happens at
        // host-block granularity (FILE_BLOCK_SIZE = 64 KB), so
        // `as u32` is safe in practice.
        let address = base
            .checked_add(offset as u32)
            .ok_or_else(|| format!("address overflow @{:#x}+{offset:#x}", base))?;
        let mut client = self.client.borrow_mut();
        let snapshot = block_on(client.run(GetMem { address, length: want as u32 }))
            .map_err(|e| format!("getmem {address:#010x}+{want}: {e}"))?;
        Ok(snapshot.data)
    }

    /// Resolve a synthetic memory-backed path to an absolute
    /// virtual address + bound, then push `data` via `setmem`.
    /// The kernel may write fewer bytes than requested (it stops
    /// at the first unmapped page); the actual count is returned
    /// so the editor can surface a partial-write to the user.
    fn write_synthetic(
        &self,
        kind: SyntheticKind<'_>,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<u64, String> {
        let (base, total_size) = match kind {
            SyntheticKind::Module(name) => {
                self.load_modules()?;
                let modules = self.modules.borrow();
                let info = modules
                    .as_ref()
                    .expect("loaded")
                    .get(name)
                    .ok_or_else(|| format!("unknown module {name:?}"))?;
                (info.base, info.size as u64)
            }
            SyntheticKind::Memory(hex) => {
                self.load_memory_regions()?;
                let regions = self.memory_regions.borrow();
                let region = regions
                    .as_ref()
                    .expect("loaded")
                    .get(hex)
                    .ok_or_else(|| format!("unknown memory region {hex:?}"))?;
                (region.base, region.size as u64)
            }
        };
        let end = offset.saturating_add(data.len() as u64);
        if end > total_size {
            return Err(format!(
                "write extends past entry size: offset={offset} len={} total={total_size}",
                data.len()
            ));
        }
        let address = base
            .checked_add(offset as u32)
            .ok_or_else(|| format!("address overflow @{:#x}+{offset:#x}", base))?;
        let mut client = self.client.borrow_mut();
        let result = block_on(client.run(SetMem { address, data }))
            .map_err(|e| format!("setmem {address:#010x}: {e}"))?;
        Ok(result.written as u64)
    }
}

/// Coarse classification of a VFS path so each callsite doesn't have
/// to re-parse. Drive-tree paths live under `/physical/<DRIVE>:/...`;
/// memory + modules under `/memory/{maps,modules}/<entry>`.
enum PathKind<'a> {
    /// `/`
    Root,
    /// `/physical`
    PhysicalRoot,
    /// `/physical/<DRIVE>:`
    #[allow(dead_code)]
    Drive(&'a str),
    /// `/physical/<DRIVE>:/<...>`
    #[allow(dead_code)]
    Path(&'a str),
    /// `/memory`
    MemoryRoot,
    /// `/memory/maps` or `/memory/modules`
    MemoryCategory,
    /// Anything else under `/memory/...` -- handled by
    /// [`classify_synthetic`] for the file-leaf paths.
    MemoryEntry,
    /// Anything we don't recognise. The mount returns errors for
    /// these from `read_dir` / `metadata`.
    #[allow(dead_code)]
    Unknown,
}

fn classify_path(path: &str) -> PathKind<'_> {
    let trimmed = path.trim_start_matches('/').trim_end_matches('/');
    if trimmed.is_empty() {
        return PathKind::Root;
    }
    if trimmed == "physical" {
        return PathKind::PhysicalRoot;
    }
    if trimmed == "memory" {
        return PathKind::MemoryRoot;
    }
    if trimmed == "memory/maps" || trimmed == "memory/modules" {
        return PathKind::MemoryCategory;
    }
    if let Some(rest) = trimmed.strip_prefix("physical/") {
        // `physical/E:` -> Drive; `physical/E:/Games[/...]` -> Path
        return match rest.split_once('/') {
            None => PathKind::Drive(rest),
            Some((drive, _)) => PathKind::Path(drive),
        };
    }
    if trimmed.starts_with("memory/") {
        return PathKind::MemoryEntry;
    }
    PathKind::Unknown
}

/// Translate a VFS path under `/physical/...` into XBDM's native
/// form (`/physical/E:/Games/Halo3.xex` -> `E:\Games\Halo3.xex`).
/// The drive root always carries the trailing `\` because the
/// XBDM `dirlist` command needs it to resolve.
fn path_to_xbdm(path: &str) -> String {
    let trimmed = path.trim_start_matches('/').trim_end_matches('/');
    let inner = trimmed.strip_prefix("physical/").unwrap_or(trimmed);
    if inner.is_empty() {
        return String::new();
    }
    let xbdm: String = inner.replace('/', "\\");
    if !xbdm.contains('\\') {
        format!("{xbdm}\\")
    } else {
        xbdm
    }
}

/// Blocking `std::net::TcpStream` exposed as
/// `futures_io::AsyncRead + AsyncWrite`. Every poll just calls the
/// underlying blocking syscall and returns `Poll::Ready(...)`; we
/// never yield `Pending`. `block_on` runs the future on the calling
/// thread and resolves it in a single poll because nothing else
/// needs to make progress -- the wasm guest IS the OS thread.
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
    fn physical_root_classifies() {
        assert!(matches!(classify_path("/physical"), PathKind::PhysicalRoot));
        assert!(matches!(classify_path("/physical/"), PathKind::PhysicalRoot));
    }

    #[test]
    fn memory_categories_classify() {
        assert!(matches!(classify_path("/memory"), PathKind::MemoryRoot));
        assert!(matches!(classify_path("/memory/maps"), PathKind::MemoryCategory));
        assert!(matches!(classify_path("/memory/modules"), PathKind::MemoryCategory));
    }

    #[test]
    fn drive_classifies_as_drive() {
        assert!(matches!(classify_path("/physical/E:"), PathKind::Drive("E:")));
    }

    #[test]
    fn path_classifies_as_path() {
        assert!(matches!(classify_path("/physical/E:/Games"), PathKind::Path("E:")));
        assert!(matches!(
            classify_path("/physical/E:/Games/Halo3.xex"),
            PathKind::Path("E:")
        ));
    }

    #[test]
    fn drive_root_path_carries_trailing_backslash() {
        assert_eq!(path_to_xbdm("/physical/E:"), "E:\\");
        assert_eq!(path_to_xbdm("/physical/E:/"), "E:\\");
    }

    #[test]
    fn nested_path_uses_backslash_separator() {
        assert_eq!(path_to_xbdm("/physical/E:/Games"), "E:\\Games");
        assert_eq!(
            path_to_xbdm("/physical/E:/Games/Halo3.xex"),
            "E:\\Games\\Halo3.xex"
        );
    }

    #[test]
    fn synthetic_memory_paths_classify() {
        assert!(matches!(
            classify_synthetic("/memory/modules/xam.xex"),
            Some(SyntheticKind::Module("xam.xex"))
        ));
        assert!(matches!(
            classify_synthetic("/memory/maps/80000000"),
            Some(SyntheticKind::Memory("80000000"))
        ));
        assert!(classify_synthetic("/memory").is_none());
        assert!(classify_synthetic("/memory/maps").is_none());
        assert!(classify_synthetic("/physical/E:").is_none());
    }
}
