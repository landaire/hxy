//! Sample VFS plugin. Matches any source and exposes it as a single
//! file at `/data.bin` -- proves the bidirectional interface works
//! (host-imported `source.read` is called on every `read_file`).

#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use hxy_plugin_api::handler::Command;
use hxy_plugin_api::handler::FileType;
use hxy_plugin_api::handler::Guest;
use hxy_plugin_api::handler::GuestCommands;
use hxy_plugin_api::handler::GuestMount;
use hxy_plugin_api::handler::InvokeResult;
use hxy_plugin_api::handler::Metadata;
use hxy_plugin_api::handler::exports::hxy::vfs::handler::MountError;
use hxy_plugin_api::handler::source;

struct Plugin;

impl Guest for Plugin {
    type Mount = Mount;

    fn matches(_head: Vec<u8>) -> bool {
        true
    }

    fn name() -> String {
        "passthrough".to_string()
    }

    fn mount_source() -> Result<hxy_plugin_api::handler::exports::hxy::vfs::handler::Mount, String> {
        Ok(hxy_plugin_api::handler::exports::hxy::vfs::handler::Mount::new(Mount))
    }

    fn mount_by_token(
        _token: String,
    ) -> Result<hxy_plugin_api::handler::exports::hxy::vfs::handler::Mount, MountError> {
        Err(MountError {
            message: "passthrough does not support token-driven mounts".to_string(),
            retry_label: None,
        })
    }
}

// No-op commands export. The world requires every plugin to
// export `commands`, but passthrough doesn't contribute any
// palette entries, so it returns an empty list and `invoke` /
// `respond` are unreachable.
impl GuestCommands for Plugin {
    fn list_commands() -> Vec<Command> {
        Vec::new()
    }

    fn invoke(_id: String) -> InvokeResult {
        InvokeResult::Done
    }

    fn respond(_id: String, _answer: String) -> InvokeResult {
        InvokeResult::Done
    }
}

struct Mount;

impl GuestMount for Mount {
    fn read_dir(&self, path: String) -> Result<Vec<String>, String> {
        match path.as_str() {
            "" | "/" => Ok(vec!["data.bin".to_string()]),
            other => Err(format!("no such dir: {other}")),
        }
    }

    fn metadata(&self, path: String) -> Result<Metadata, String> {
        match path.as_str() {
            "" | "/" => Ok(Metadata { file_type: FileType::Directory, length: 0 }),
            "/data.bin" => Ok(Metadata { file_type: FileType::RegularFile, length: source::len() }),
            other => Err(format!("no such path: {other}")),
        }
    }

    fn read_file(&self, path: String) -> Result<Vec<u8>, String> {
        if path != "/data.bin" {
            return Err(format!("no such file: {path}"));
        }
        source::read(0, source::len())
    }

    fn read_range(&self, path: String, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        if path != "/data.bin" {
            return Err(format!("no such file: {path}"));
        }
        // Passthrough is just a renamed view of the source bytes;
        // we route the ranged read straight through the host
        // import without ever materialising the whole file.
        let total = source::len();
        let end = offset.saturating_add(length).min(total);
        let len = end.saturating_sub(offset);
        if len == 0 {
            return Ok(Vec::new());
        }
        source::read(offset, len)
    }

    fn write_range(&self, _path: String, _offset: u64, _data: Vec<u8>) -> Result<u64, String> {
        // Source files are read-only -- the user opened the byte
        // buffer for inspection, not editing.
        Err("passthrough mount is read-only".to_string())
    }
}

hxy_plugin_api::handler::export_handler!(Plugin with_types_in hxy_plugin_api::handler);
