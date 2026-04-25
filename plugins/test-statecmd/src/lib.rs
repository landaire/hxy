//! Test fixture plugin. Exists to give the host an end-to-end target
//! it can drive over the real WIT bindings without needing a
//! production plugin to exist.
//!
//! Exports the `commands` interface with three entries that
//! correspond to the three [`InvokeResult`] variants, plus the
//! mandatory handler-interface stubs (`mount-source` / `mount-by-token`
//! both return errors -- this fixture has no real VFS surface).
//!
//! Uses the `state` import as a counter: every `invoke` reads the
//! current value, increments it, writes it back. The host can use
//! that observable behaviour to verify that persist gating + the
//! state-store wiring round-trips correctly.

// Needs `std` for `std::net::TcpStream` (wasi-sockets via wasm32-wasip2).
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use hxy_plugin_api::handler::Command;
use hxy_plugin_api::handler::Guest;
use hxy_plugin_api::handler::GuestCommands;
use hxy_plugin_api::handler::GuestMount;
use hxy_plugin_api::handler::InvokeResult;
use hxy_plugin_api::handler::Metadata;
use hxy_plugin_api::handler::MountRequest;
use hxy_plugin_api::handler::PromptRequest;
use hxy_plugin_api::handler::state;
use std::io::Read;
use std::io::Write;
use std::net::TcpStream;

/// Wire tag for the `network` command's saved-state blob. The
/// integration test reads the blob via `StateStore::load` and asserts
/// against the first byte; this avoids brittle string-matching
/// against `io::Error::to_string()` across platforms / wasi versions.
///
/// Layout: `[tag, ..rest]`.
/// - [`STATE_TAG_OK`]:     `[OK,     ..response bytes]`
/// - [`STATE_TAG_DENIED`]: `[DENIED, ..ignored]` (no message body)
/// - [`STATE_TAG_OTHER`]:  `[OTHER,  ..utf-8 error message]`
pub const STATE_TAG_OK: u8 = 0x00;
pub const STATE_TAG_DENIED: u8 = 0x01;
pub const STATE_TAG_OTHER: u8 = 0x02;

/// Typed error returned by [`tcp_roundtrip`]. `Denied` is the
/// permission-denied case wasi-sockets surfaces when the
/// manifest's `network` allowlist doesn't cover the requested
/// destination; `Other` is everything else (refused, unreachable,
/// truncated, ...).
#[derive(Debug)]
enum NetError {
    Denied,
    Other(String),
}

impl From<std::io::Error> for NetError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::PermissionDenied => NetError::Denied,
            _ => NetError::Other(e.to_string()),
        }
    }
}

impl NetError {
    fn encode(&self) -> Vec<u8> {
        match self {
            NetError::Denied => vec![STATE_TAG_DENIED],
            NetError::Other(msg) => {
                let mut out = Vec::with_capacity(1 + msg.len());
                out.push(STATE_TAG_OTHER);
                out.extend_from_slice(msg.as_bytes());
                out
            }
        }
    }
}

struct Plugin;

impl Guest for Plugin {
    type Mount = Mount;

    fn matches(_head: Vec<u8>) -> bool {
        // Never claim ordinary file sources; this fixture exists for
        // the palette / state path, not for filesystem detection.
        false
    }

    fn name() -> String {
        "test-statecmd".to_string()
    }

    fn mount_source() -> Result<hxy_plugin_api::handler::exports::hxy::vfs::handler::Mount, String> {
        Err("test-statecmd does not expose a file mount".to_string())
    }

    fn mount_by_token(token: String) -> Result<hxy_plugin_api::handler::exports::hxy::vfs::handler::Mount, String> {
        // Use the token as the synthetic "session id" so the host's
        // tab title can include it for visual confirmation that the
        // token round-tripped intact.
        Ok(hxy_plugin_api::handler::exports::hxy::vfs::handler::Mount::new(Mount { token }))
    }
}

impl GuestCommands for Plugin {
    fn list_commands() -> Vec<Command> {
        vec![
            Command {
                id: "done".to_string(),
                label: "Done outcome".to_string(),
                subtitle: Some(format!("counter = {}", load_counter())),
                icon: None,
                has_children: false,
            },
            Command {
                id: "cascade".to_string(),
                label: "Cascade outcome".to_string(),
                subtitle: None,
                icon: None,
                has_children: true,
            },
            Command {
                id: "mount".to_string(),
                label: "Mount outcome".to_string(),
                subtitle: None,
                icon: None,
                has_children: false,
            },
            Command {
                id: "prompt".to_string(),
                label: "Prompt outcome".to_string(),
                subtitle: Some("type a token name; mounts it back".to_string()),
                icon: None,
                has_children: false,
            },
            Command {
                id: "network".to_string(),
                label: "Network roundtrip".to_string(),
                subtitle: Some("connect to host:port, send 'ping', read echo".to_string()),
                icon: None,
                has_children: false,
            },
        ]
    }

    fn invoke(id: String) -> InvokeResult {
        // Bump the state counter on every invoke so the host can
        // observe persistence by looking at the subtitle in the
        // next list_commands call.
        let next = bump_counter();
        match id.as_str() {
            "done" => InvokeResult::Done,
            "cascade" => InvokeResult::Cascade(vec![
                Command {
                    id: "child-a".to_string(),
                    label: format!("Child A (counter = {next})"),
                    subtitle: None,
                    icon: None,
                    has_children: false,
                },
                Command {
                    id: "child-b".to_string(),
                    label: format!("Child B (counter = {next})"),
                    subtitle: None,
                    icon: None,
                    has_children: false,
                },
            ]),
            "mount" => InvokeResult::Mount(MountRequest {
                token: format!("token-{next}"),
                title: format!("Test mount #{next}"),
            }),
            "prompt" => InvokeResult::Prompt(PromptRequest {
                title: "Token name".to_string(),
                default_value: Some(format!("default-{next}")),
            }),
            "network" => InvokeResult::Prompt(PromptRequest {
                title: "host:port to connect to".to_string(),
                default_value: None,
            }),
            // Children of `cascade` resolve as Done -- they exist
            // so the host can verify cascade dispatch routes back
            // to the right plugin.
            _ => InvokeResult::Done,
        }
    }

    fn respond(id: String, answer: String) -> InvokeResult {
        match id.as_str() {
            // Answer to the "prompt" command becomes a Mount so
            // the host can verify the typed string round-trips
            // back through into a token-driven tab.
            "prompt" => InvokeResult::Mount(MountRequest {
                token: format!("from-prompt:{answer}"),
                title: format!("Prompt answered: {answer}"),
            }),
            // Answer to "network" is a host:port. Parse it,
            // connect via the host's tcp interface, send "ping",
            // read up to 64 bytes, store the answer in state for
            // the test to inspect, return Done.
            "network" => {
                let blob = match tcp_roundtrip(&answer) {
                    Ok(bytes) => {
                        let mut out = Vec::with_capacity(1 + bytes.len());
                        out.push(STATE_TAG_OK);
                        out.extend_from_slice(&bytes);
                        out
                    }
                    Err(e) => e.encode(),
                };
                let _ = state::save(&blob);
                InvokeResult::Done
            }
            _ => InvokeResult::Done,
        }
    }
}

/// Connect to `host:port`, write "ping", read up to 64 bytes back.
/// Returns the echoed bytes on success, or a typed [`NetError`]
/// distinguishing "host denied connect" from anything else.
///
/// Uses `std::net::TcpStream` directly; on `wasm32-wasip2` that
/// resolves to wasi-sockets imports satisfied by the host's
/// `wasmtime-wasi` linker (gated by the manifest's `network`
/// allowlist).
fn tcp_roundtrip(host_port: &str) -> Result<Vec<u8>, NetError> {
    let mut stream = TcpStream::connect(host_port)?;
    stream.write_all(b"ping")?;
    let mut buf = vec![0u8; 64];
    let n = stream.read(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

struct Mount {
    token: String,
}

impl GuestMount for Mount {
    fn read_dir(&self, path: String) -> Result<Vec<String>, String> {
        match path.as_str() {
            "" | "/" => Ok(vec![format!("{}.txt", self.token)]),
            other => Err(format!("no such dir: {other}")),
        }
    }

    fn metadata(&self, path: String) -> Result<Metadata, String> {
        if path == "/" || path.is_empty() {
            return Ok(Metadata { file_type: hxy_plugin_api::handler::FileType::Directory, length: 0 });
        }
        let expected = format!("/{}.txt", self.token);
        if path == expected {
            let len = self.token.len() as u64;
            Ok(Metadata { file_type: hxy_plugin_api::handler::FileType::RegularFile, length: len })
        } else {
            Err(format!("no such path: {path}"))
        }
    }

    fn read_file(&self, path: String) -> Result<Vec<u8>, String> {
        let expected = format!("/{}.txt", self.token);
        if path == expected {
            Ok(self.token.as_bytes().to_vec())
        } else {
            Err(format!("no such file: {path}"))
        }
    }

    fn read_range(&self, path: String, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        let body = self.read_file(path)?;
        let start = (offset as usize).min(body.len());
        let end = ((offset.saturating_add(length)) as usize).min(body.len());
        Ok(body[start..end].to_vec())
    }
}

/// Read the persisted counter, defaulting to zero when the state
/// blob is empty / absent / the persist permission was denied.
/// `state::load` returning `Err(Denied)` is treated identically to
/// "no value yet": the plugin keeps working, just without memory
/// across calls.
fn load_counter() -> u32 {
    let bytes = match state::load() {
        Ok(Some(b)) if b.len() >= 4 => b,
        _ => return 0,
    };
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// Increment the persisted counter and return its new value.
/// Failures (denied, quota, etc.) are swallowed -- the test
/// fixture isn't trying to advertise its own errors; it's the
/// host's job to verify the state path does what it should.
fn bump_counter() -> u32 {
    let next = load_counter().saturating_add(1);
    let _ = state::save(&next.to_le_bytes());
    next
}

hxy_plugin_api::handler::export_handler!(Plugin with_types_in hxy_plugin_api::handler);
