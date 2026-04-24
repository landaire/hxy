//! Single-instance IPC. The first hxy process to start binds a
//! local socket; subsequent invocations connect to it and forward
//! their CLI file paths instead of opening a fresh window.
//!
//! Cross-platform via the `interprocess` crate's
//! [`GenericNamespaced`] name type: a Linux abstract socket, a
//! `/tmp/hxy.sock`-style file on macOS, and a `\\.\pipe\hxy.sock`
//! named pipe on Windows -- one constant covers all three.
//!
//! Wire format is a `u32` little-endian length prefix followed by an
//! rkyv-archived [`IpcMessage`]. Sized framing means the receiver
//! always knows how much to read up front; rkyv gives us a typed,
//! versionable schema so adding a new variant later (e.g. "raise
//! window", "open in split") doesn't require redesigning the parser.

#![cfg(not(target_arch = "wasm32"))]

use std::io;
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;
use std::thread;

use interprocess::local_socket::GenericNamespaced;
use interprocess::local_socket::ListenerOptions;
use interprocess::local_socket::Name;
use interprocess::local_socket::ToNsName;
use interprocess::local_socket::traits::Listener as ListenerTrait;
use interprocess::local_socket::traits::Stream as StreamTrait;
use rkyv::Archive;
use rkyv::Deserialize;
use rkyv::Serialize;
use rkyv::rancor;
use rkyv::util::AlignedVec;

/// Stable socket name used by both client and server. Kept short to
/// stay within Unix sun_path limits when `GenericNamespaced` falls
/// back to a filesystem path on macOS.
const SOCKET_NAME: &str = "hxy.sock";

/// Reject framed messages claiming more than this many bytes. A
/// stuffed-but-honest "open every file in /" payload would be a few
/// hundred KiB; anything past this is almost certainly a protocol
/// confusion or a malicious peer trying to make us allocate the
/// world.
const MAX_MESSAGE_BYTES: u32 = 16 * 1024 * 1024;

/// Typed IPC payload, archived/deserialized with rkyv. Add variants
/// here to grow the protocol -- the length-prefixed framing means
/// older receivers reading a newer message just see an `Err` from
/// validation and drop the connection (forward incompatibility is
/// caller's problem; we don't pretend to be wire-compatible across
/// versions of this enum).
#[derive(Archive, Serialize, Deserialize, Debug)]
pub enum IpcMessage {
    /// Open these absolute paths in the running instance. Strings
    /// rather than `PathBuf` because rkyv's stock derives don't
    /// archive `PathBuf`; the receiver wraps each in `PathBuf::from`
    /// after decoding, which is lossless on every platform we ship.
    Open { paths: Vec<String> },
}

fn socket_name() -> io::Result<Name<'static>> {
    SOCKET_NAME.to_ns_name::<GenericNamespaced>()
}

/// Try to forward `paths` to an already-running hxy. Returns `Ok` if
/// the bytes hit the running instance's socket; `Err` (any kind)
/// means the caller should proceed to start its own GUI. Callers
/// MUST send absolute paths -- the receiving process resolves them
/// against its own filesystem, not the sender's CWD.
pub fn try_send_to_running_instance(paths: &[PathBuf]) -> io::Result<()> {
    let msg = IpcMessage::Open {
        // `to_string_lossy` matches the user's command-line input on
        // every reasonable platform. A path with embedded NUL bytes
        // wouldn't have made it through argv either, so we don't try
        // to defend against that here.
        paths: paths.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
    };
    let bytes = rkyv::to_bytes::<rancor::Error>(&msg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let len = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "ipc message too large"))?;

    let mut stream = interprocess::local_socket::Stream::connect(socket_name()?)?;
    stream.write_all(&len.to_le_bytes())?;
    stream.write_all(&bytes)?;
    stream.flush()?;
    Ok(())
}

/// Bind the IPC socket and spawn a listener thread that forwards
/// incoming path batches to the UI via the returned inbox. Returns
/// `None` when the socket can't be bound (a stale lock, permissions,
/// etc.); the GUI still runs but won't accept forwarded opens until
/// next launch.
///
/// The listener thread runs for the lifetime of the process and is
/// torn down when the process exits.
pub fn start_server(ctx: &egui::Context) -> Option<egui_inbox::UiInbox<Vec<PathBuf>>> {
    let name = match socket_name() {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, "ipc: build socket name; CLI forwarding disabled");
            return None;
        }
    };
    // `reclaim_name` overwrites a stale Unix socket file left behind
    // by a previous crashed instance. A *live* peer at the same name
    // would have already answered our `try_send_to_running_instance`
    // probe before we got here, so it's safe to reclaim now.
    let listener = match ListenerOptions::new().name(name).reclaim_name(true).create_sync() {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, "ipc: bind socket; CLI forwarding disabled");
            return None;
        }
    };
    let (sender, inbox) = egui_inbox::UiInbox::channel_with_ctx(ctx);
    thread::spawn(move || {
        loop {
            let mut stream = match listener.accept() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "ipc: accept");
                    continue;
                }
            };
            let msg = match read_message(&mut stream) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(error = %e, "ipc: read message");
                    continue;
                }
            };
            match msg {
                IpcMessage::Open { paths } => {
                    let paths: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
                    if paths.is_empty() {
                        continue;
                    }
                    if sender.send(paths).is_err() {
                        // UI dropped the inbox -- the app is shutting down.
                        return;
                    }
                }
            }
        }
    });
    Some(inbox)
}

/// Read one length-prefixed `IpcMessage` from `stream`. Errors map
/// to `io::Error` so the caller's logging treats wire-format faults
/// the same as transport faults.
fn read_message<S: Read>(stream: &mut S) -> io::Result<IpcMessage> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("ipc message too large: {len} bytes"),
        ));
    }
    // Aligned buffer so rkyv's checked access doesn't reject the
    // payload for being misaligned; a plain `Vec<u8>` is only
    // u8-aligned and rkyv's archived layouts demand at least 16.
    let mut buf = AlignedVec::<16>::with_capacity(len as usize);
    buf.resize(len as usize, 0);
    stream.read_exact(&mut buf)?;
    rkyv::from_bytes::<IpcMessage, rancor::Error>(&buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Frame an `IpcMessage` exactly the way the live sender does so
    /// the test exercises the same length-prefix + rkyv path.
    fn frame(msg: &IpcMessage) -> Vec<u8> {
        let bytes = rkyv::to_bytes::<rancor::Error>(msg).unwrap();
        let len = bytes.len() as u32;
        let mut out = Vec::with_capacity(4 + bytes.len());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&bytes);
        out
    }

    #[test]
    fn roundtrips_open_message() {
        let original = IpcMessage::Open {
            paths: vec!["/tmp/a.bin".to_owned(), "/tmp/b.bin".to_owned()],
        };
        let wire = frame(&original);
        let mut cur = Cursor::new(wire);
        let decoded = read_message(&mut cur).expect("decode");
        let IpcMessage::Open { paths } = decoded;
        assert_eq!(paths, vec!["/tmp/a.bin".to_owned(), "/tmp/b.bin".to_owned()]);
    }

    #[test]
    fn rejects_oversize_length_prefix() {
        // Claim a message larger than MAX_MESSAGE_BYTES without
        // sending the body; read should fail before allocating.
        let mut wire = Vec::new();
        wire.extend_from_slice(&(MAX_MESSAGE_BYTES + 1).to_le_bytes());
        let mut cur = Cursor::new(wire);
        let err = read_message(&mut cur).expect_err("should reject oversized length");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_garbage_payload() {
        // Length says 16 bytes follow, but they're junk -- rkyv's
        // checked access should turn that into InvalidData.
        let mut wire = Vec::new();
        wire.extend_from_slice(&16u32.to_le_bytes());
        wire.extend_from_slice(&[0xFFu8; 16]);
        let mut cur = Cursor::new(wire);
        let err = read_message(&mut cur).expect_err("should reject garbage");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
