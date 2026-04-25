//! Xbox 360 devkit "neighborhood" plugin.
//!
//! Surfaces a "Connect to Xbox console" command in the host's command
//! palette. Picking it prompts for the kit's `host:port`, then sends
//! an XBDM NAP `WhatIsYourName` (`0x03`) packet to that address and
//! reports whatever name the console replies with.
//!
//! WASI preview 2's `wasi:sockets` doesn't support UDP broadcast
//! (subnet-wide auto-discovery isn't in the spec), so the user
//! supplies the IP. The wire framing is handled by
//! [`xeedee::Discovery`] (sans-io state machine) configured for a
//! unicast probe; this plugin only owns the UDP socket and the
//! polling loop.
//!
//! Picking a discovered console currently just closes the palette --
//! the actual TCP connect (via `xeedee::ClientEngine` over a
//! blocking TCP shim) is the next milestone.

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::net::UdpSocket;
use std::time::Duration;
use std::time::Instant;

use hxy_plugin_api::handler::Command;
use hxy_plugin_api::handler::Guest;
use hxy_plugin_api::handler::GuestCommands;
use hxy_plugin_api::handler::GuestMount;
use hxy_plugin_api::handler::InvokeResult;
use hxy_plugin_api::handler::Metadata;
use hxy_plugin_api::handler::MountRequest;
use hxy_plugin_api::handler::PromptRequest;
use xeedee::discovery::DiscoveryAction;
use xeedee::discovery::{DiscoveredConsole, Discovery, DiscoveryConfig};

/// Default text the prompt pre-fills with. Picked as a typical
/// home-router devkit address; users edit it to their kit's IP.
const PROMPT_DEFAULT: &str = "192.168.1.50:730";

/// Receive buffer for inbound NAP replies. NAP responses are at most
/// 256 bytes (`0x02 <namelen=u8> <name...>` with `namelen <= 254`),
/// so 1 KiB has plenty of headroom for any malformed reply we'd
/// silently drop anyway.
const RECV_BUF: usize = 1024;

struct Plugin;

impl Guest for Plugin {
    type Mount = NoMount;

    fn matches(_head: Vec<u8>) -> bool {
        // This plugin only contributes palette commands. It never
        // claims to handle a byte source -- detection always falls
        // through to other handlers.
        false
    }

    fn name() -> String {
        "xbox-neighborhood".to_string()
    }

    fn mount_source() -> Result<hxy_plugin_api::handler::exports::hxy::vfs::handler::Mount, String> {
        Err("xbox-neighborhood does not expose a file mount".to_string())
    }

    fn mount_by_token(
        _token: String,
    ) -> Result<hxy_plugin_api::handler::exports::hxy::vfs::handler::Mount, String> {
        // The lookup cascade resolves to `Done` for now, so the host
        // never asks us to mount by token. If a future "Connect"
        // outcome wires through a token-backed VFS over the XBDM
        // file-transfer commands, this is where we'd open it.
        Err("xbox-neighborhood does not yet support token-backed mounts".to_string())
    }
}

impl GuestCommands for Plugin {
    fn list_commands() -> Vec<Command> {
        vec![Command {
            id: "connect".to_string(),
            label: "Connect to Xbox console".to_string(),
            subtitle: Some("NAP unicast probe -- prompts for the kit IP".to_string()),
            icon: None,
            // Picking opens a prompt rather than a sub-menu, so the
            // cosmetic hint stays false.
            has_children: false,
        }]
    }

    fn invoke(id: String) -> InvokeResult {
        match id.as_str() {
            "connect" => InvokeResult::Prompt(PromptRequest {
                title: "Xbox console (host:port)".to_string(),
                default_value: Some(PROMPT_DEFAULT.to_string()),
            }),
            // A picked console entry from the cascade we returned
            // earlier. For the MVP we just close the palette; the
            // next milestone is to open a TCP session and surface
            // a Mount with the console's filesystem.
            picked if picked.starts_with("console:") => InvokeResult::Done,
            _ => InvokeResult::Done,
        }
    }

    fn respond(id: String, answer: String) -> InvokeResult {
        if id != "connect" {
            return InvokeResult::Done;
        }
        match probe_console(&answer) {
            Ok(console) => InvokeResult::Cascade(vec![Command {
                id: format!("console:{}@{}", console.name, console.addr),
                label: console.name.clone(),
                subtitle: Some(console.addr.to_string()),
                icon: None,
                has_children: false,
            }]),
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

/// Stub `mount` resource. Required by the WIT trait but never
/// actually constructed: `Plugin::mount_source` and `mount_by_token`
/// always return `Err`.
struct NoMount;

impl GuestMount for NoMount {
    fn read_dir(&self, _path: String) -> Result<Vec<String>, String> {
        Err("xbox-neighborhood has no mount surface".to_string())
    }
    fn metadata(&self, _path: String) -> Result<Metadata, String> {
        Err("xbox-neighborhood has no mount surface".to_string())
    }
    fn read_file(&self, _path: String) -> Result<Vec<u8>, String> {
        Err("xbox-neighborhood has no mount surface".to_string())
    }
}

/// Send a unicast NAP probe to `host_port` and return the console
/// that answered, if any. Errors are joined as strings so the
/// palette has a single failure surface.
fn probe_console(host_port: &str) -> Result<DiscoveredConsole, String> {
    let target = resolve_target(host_port)?;
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind: {e}"))?;
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
            DiscoveryAction::Wait { until } => {
                let now = Instant::now();
                let budget = until.saturating_duration_since(now);
                let timeout = if budget.is_zero() {
                    Duration::from_millis(1)
                } else {
                    budget
                };
                socket
                    .set_read_timeout(Some(timeout))
                    .map_err(|e| format!("set_read_timeout: {e}"))?;
                match socket.recv_from(&mut buf) {
                    Ok((n, src)) => engine.handle_inbound(src, &buf[..n]),
                    Err(e) if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {
                        // Timer fired -- loop and re-poll so the
                        // state machine sees the deadline.
                    }
                    Err(e) => return Err(format!("recv_from: {e}")),
                }
            }
        }
    }
}

/// Parse `host:port` (default port 730 if omitted), resolving via
/// the OS resolver so DNS names work in addition to bare IPs.
fn resolve_target(host_port: &str) -> Result<SocketAddr, String> {
    // Try the input as `host:port` first; fall back to treating the
    // input as a bare host and using the default XBDM port.
    let with_port = host_port.to_string();
    let candidates: Vec<String> = if host_port.contains(':') {
        vec![with_port]
    } else {
        vec![format!("{host_port}:730")]
    };
    for candidate in &candidates {
        if let Ok(mut addrs) = candidate.as_str().to_socket_addrs()
            && let Some(addr) = addrs.next()
        {
            return Ok(addr);
        }
    }
    Err(format!("could not resolve {host_port:?}"))
}

// Reference imports the body doesn't directly name so the compiler
// keeps the use-statements live for documentation while warnings stay
// clean.
#[allow(dead_code)]
fn _unused_imports(_: MountRequest) {}

hxy_plugin_api::handler::export_handler!(Plugin with_types_in hxy_plugin_api::handler);
