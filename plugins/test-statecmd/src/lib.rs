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

#![no_std]
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
use hxy_plugin_api::handler::state;

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
            // Children of `cascade` resolve as Done -- they exist
            // so the host can verify cascade dispatch routes back
            // to the right plugin.
            _ => InvokeResult::Done,
        }
    }
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
