//! Sample VFS plugin. Matches any source and exposes it as a single
//! file at `/data.bin` — proves the bidirectional interface works
//! (host-imported `source.read` is called on every `read_file`).

#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use hxy_plugin_api::FileType;
use hxy_plugin_api::Guest;
use hxy_plugin_api::GuestMount;
use hxy_plugin_api::Metadata;
use hxy_plugin_api::source;

struct Plugin;

impl Guest for Plugin {
    type Mount = Mount;

    fn matches(_head: Vec<u8>) -> bool {
        true
    }

    fn name() -> String {
        "passthrough".to_string()
    }

    fn mount_source() -> Result<hxy_plugin_api::exports::hxy::vfs::handler::Mount, String> {
        Ok(hxy_plugin_api::exports::hxy::vfs::handler::Mount::new(Mount))
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
}

hxy_plugin_api::export_plugin!(Plugin with_types_in hxy_plugin_api);
