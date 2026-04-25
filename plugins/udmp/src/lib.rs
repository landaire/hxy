//! Windows minidump VFS plugin.
//!
//! Mounts a `.dmp` byte source as a tree of byte-level views over the
//! parts that are reasonable to hex edit: the file header, individual
//! streams, every thread's context + stack, every module's metadata
//! records, and every captured memory region.
//!
//! Layout summary:
//!
//! ```text
//! /
//! ├── header                        # MINIDUMP_HEADER (32 bytes at file offset 0)
//! ├── streams/
//! │   ├── system_info               # SystemInfoStream raw
//! │   └── exception                 # ExceptionStream raw (if present)
//! ├── threads/
//! │   └── <tid>/
//! │       ├── entry                 # ThreadEntry struct
//! │       ├── context               # raw thread context (x86: 716 bytes, x64: 1232)
//! │       └── stack                 # the thread's stack memory (when captured)
//! ├── modules/
//! │   └── <basename>/
//! │       ├── entry                 # ModuleEntry struct
//! │       ├── version_info          # FixedFileInfo substruct
//! │       ├── name                  # UTF-16LE module path (no length prefix)
//! │       ├── cv_record             # CodeView record (PDB info)
//! │       └── misc_record           # MISC record (when present)
//! └── memory/
//!     └── 0x<base>                  # one regular file per captured memory range
//! ```
//!
//! Memory region offsets are recovered from `udmp_parser`'s `MemBlock::data`
//! slice via pointer arithmetic against the input buffer; everything
//! else is parsed directly from the dump bytes so we know the source-
//! file offset of each emitted file (the udmp-parser API exposes parsed
//! values, not file offsets, for thread contexts and module entries).
//! Read-only: edits land in the editor's patch overlay but writeback is
//! rejected.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;

use hxy_plugin_api::handler::Command;
use hxy_plugin_api::handler::FileType;
use hxy_plugin_api::handler::Guest;
use hxy_plugin_api::handler::GuestCommands;
use hxy_plugin_api::handler::GuestMount;
use hxy_plugin_api::handler::InvokeResult;
use hxy_plugin_api::handler::Metadata;
use hxy_plugin_api::handler::source;

use udmp_parser::UserDumpParser;

const MDMP_SIG: [u8; 4] = [b'M', b'D', b'M', b'P'];

const STREAM_THREAD_LIST: u32 = 3;
const STREAM_MODULE_LIST: u32 = 4;
const STREAM_EXCEPTION: u32 = 6;
const STREAM_SYSTEM_INFO: u32 = 7;

const HEADER_SIZE: u64 = 32;
const DIRECTORY_ENTRY_SIZE: u64 = 12;
const THREAD_ENTRY_SIZE: u64 = 48;
const MODULE_ENTRY_SIZE: u64 = 108;
const FIXED_FILE_INFO_SIZE: u64 = 52;
/// Minimum sane size for a SystemInfoStream record we emit. The on-
/// disk record can be larger when service-pack strings or processor-
/// feature blocks tail it, but we always claim at least this many
/// bytes so the user sees the core fields.
const SYSTEM_INFO_BASE_SIZE: u64 = 32;

struct Plugin;

impl Guest for Plugin {
    type Mount = Mount;

    fn matches(head: Vec<u8>) -> bool {
        head.len() >= 4 && head[0..4] == MDMP_SIG
    }

    fn name() -> String {
        "udmp".to_string()
    }

    fn mount_source() -> Result<hxy_plugin_api::handler::exports::hxy::vfs::handler::Mount, String> {
        let mount = Mount::build()?;
        Ok(hxy_plugin_api::handler::exports::hxy::vfs::handler::Mount::new(mount))
    }

    fn mount_by_token(_token: String) -> Result<hxy_plugin_api::handler::exports::hxy::vfs::handler::Mount, String> {
        Err("udmp does not support token-driven mounts".to_string())
    }
}

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

#[derive(Clone, Copy, Debug)]
struct FileNode {
    rva: u64,
    length: u64,
}

struct Mount {
    files: BTreeMap<String, FileNode>,
    dirs: BTreeMap<String, BTreeSet<String>>,
}

impl Mount {
    fn build() -> Result<Self, String> {
        let total = source::len();
        if total < HEADER_SIZE {
            return Err(format!("dump is {} bytes; need at least {}", total, HEADER_SIZE));
        }
        let bytes = source::read(0, total).map_err(|e| format!("read source: {e}"))?;
        if bytes.len() as u64 != total {
            return Err(format!("short read: {} of {}", bytes.len(), total));
        }
        if bytes[0..4] != MDMP_SIG {
            return Err("not a minidump (missing MDMP signature)".to_string());
        }

        // Validate via udmp-parser. The parser borrows `bytes`; we only
        // use it to enumerate memory regions (where slice pointer
        // arithmetic recovers offsets that aren't otherwise exposed).
        let parser = UserDumpParser::with_slice(&bytes).map_err(|e| format!("parse minidump: {e}"))?;

        let mut mount = Self { files: BTreeMap::new(), dirs: BTreeMap::new() };

        mount.add_file("/header", 0, HEADER_SIZE);

        let n_streams = read_u32(&bytes, 8).ok_or_else(|| "truncated header".to_string())?;
        let stream_dir_rva = read_u32(&bytes, 12).ok_or_else(|| "truncated header".to_string())?;

        for i in 0..n_streams as u64 {
            let dir_off = stream_dir_rva as u64 + i * DIRECTORY_ENTRY_SIZE;
            let stream_type = read_u32(&bytes, dir_off as usize)
                .ok_or_else(|| format!("truncated stream directory @ {dir_off:#x}"))?;
            let data_size = read_u32(&bytes, (dir_off + 4) as usize)
                .ok_or_else(|| "truncated location descriptor".to_string())?;
            let rva = read_u32(&bytes, (dir_off + 8) as usize)
                .ok_or_else(|| "truncated location descriptor".to_string())?;
            match stream_type {
                STREAM_SYSTEM_INFO => {
                    // System info has a tail (CSD version string + processor
                    // feature info) we don't surface separately. Expose the
                    // whole declared range so the user sees everything.
                    let len = (data_size as u64).max(SYSTEM_INFO_BASE_SIZE);
                    mount.add_file("/streams/system_info", rva as u64, len);
                }
                STREAM_EXCEPTION => {
                    mount.add_file("/streams/exception", rva as u64, data_size as u64);
                }
                STREAM_THREAD_LIST => {
                    parse_thread_list(&mut mount, &bytes, rva as u64)?;
                }
                STREAM_MODULE_LIST => {
                    parse_module_list(&mut mount, &bytes, rva as u64)?;
                }
                _ => {}
            }
        }

        // Memory regions: udmp-parser already associated each MemBlock with
        // its data slice. The slice points into our `bytes` buffer, so the
        // file offset is `data.as_ptr() - bytes.as_ptr()`.
        let bytes_base = bytes.as_ptr() as usize;
        for block in parser.mem_blocks().values() {
            if block.data.is_empty() {
                continue;
            }
            let block_off = block.data.as_ptr() as usize;
            // Defensive: reject pointers outside our buffer rather than
            // emit a bogus offset.
            if block_off < bytes_base || block_off > bytes_base + bytes.len() {
                continue;
            }
            let rva = (block_off - bytes_base) as u64;
            let length = block.data.len() as u64;
            let path = format!("/memory/0x{:016X}", block.range.start);
            mount.add_file(&path, rva, length);
        }

        // Drop the parser before returning so the borrow on `bytes` ends
        // and `bytes` itself drops at end of scope. We don't keep the
        // bytes around; reads route back through `source::read`.
        drop(parser);

        Ok(mount)
    }

    fn add_file(&mut self, path: &str, rva: u64, length: u64) {
        self.files.insert(path.to_string(), FileNode { rva, length });

        // Walk parent components and register each in the dirs map so
        // read_dir / metadata can resolve intermediate directories
        // without us having to enumerate them explicitly.
        let mut p = path;
        while let Some(idx) = p.rfind('/') {
            let parent = if idx == 0 { "/" } else { &p[..idx] };
            let name = &p[idx + 1..];
            self.dirs.entry(parent.to_string()).or_default().insert(name.to_string());
            if idx == 0 {
                break;
            }
            p = &p[..idx];
        }
    }

    fn lookup(&self, path: &str) -> Option<FileNode> {
        self.files.get(path).copied()
    }

    fn is_dir(&self, path: &str) -> bool {
        self.dirs.contains_key(path)
    }
}

impl GuestMount for Mount {
    fn read_dir(&self, path: String) -> Result<Vec<String>, String> {
        let normalized = if path.is_empty() { "/" } else { path.as_str() };
        match self.dirs.get(normalized) {
            Some(children) => Ok(children.iter().cloned().collect()),
            None => Err(format!("no such directory: {normalized}")),
        }
    }

    fn metadata(&self, path: String) -> Result<Metadata, String> {
        let normalized = if path.is_empty() { "/" } else { path.as_str() };
        if self.is_dir(normalized) {
            return Ok(Metadata { file_type: FileType::Directory, length: 0 });
        }
        if let Some(node) = self.lookup(normalized) {
            return Ok(Metadata { file_type: FileType::RegularFile, length: node.length });
        }
        Err(format!("no such path: {normalized}"))
    }

    fn read_file(&self, path: String) -> Result<Vec<u8>, String> {
        let node = self.lookup(&path).ok_or_else(|| format!("no such file: {path}"))?;
        if node.length == 0 {
            return Ok(Vec::new());
        }
        source::read(node.rva, node.length).map_err(|e| format!("source.read: {e}"))
    }

    fn read_range(&self, path: String, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        let node = self.lookup(&path).ok_or_else(|| format!("no such file: {path}"))?;
        if offset >= node.length || length == 0 {
            return Ok(Vec::new());
        }
        let clamped = length.min(node.length - offset);
        source::read(node.rva + offset, clamped).map_err(|e| format!("source.read: {e}"))
    }

    fn write_range(&self, _path: String, _offset: u64, _data: Vec<u8>) -> Result<u64, String> {
        // The plugin parses a static byte source -- writebacks would
        // need a way to push edits through the host's source channel,
        // which the WIT only exposes for reads. Mirror passthrough's
        // posture: edits in the editor's patch overlay still work, but
        // saving in-place is rejected.
        Err("udmp mount is read-only".to_string())
    }
}

/// Parse the thread list at `stream_rva` and emit `/threads/<tid>/...`
/// entries. The on-disk layout is:
///
/// * `u32 number_of_threads`
/// * `ThreadEntry[number_of_threads]` (48 bytes each, see [`THREAD_ENTRY_SIZE`])
fn parse_thread_list(mount: &mut Mount, bytes: &[u8], stream_rva: u64) -> Result<(), String> {
    let n_threads = read_u32(bytes, stream_rva as usize).ok_or_else(|| "truncated thread list header".to_string())?;
    let entries_start = stream_rva + 4;
    for i in 0..n_threads as u64 {
        let entry_off = entries_start + i * THREAD_ENTRY_SIZE;
        let tid = read_u32(bytes, entry_off as usize)
            .ok_or_else(|| format!("truncated thread entry @ {entry_off:#x}"))?;
        // ThreadEntry layout (offsets within the entry):
        //   0  thread_id u32
        //   4  suspend_count u32
        //   8  priority_class u32
        //  12  priority u32
        //  16  teb u64
        //  24  stack: MemoryDescriptor { start_of_memory_range u64, memory: LocationDescriptor32 { data_size u32 (32), rva u32 (36) } }
        //  40  thread_context: LocationDescriptor32 { data_size u32 (40), rva u32 (44) }
        let stack_size = read_u32(bytes, (entry_off + 32) as usize).ok_or("truncated stack descriptor")?;
        let stack_rva = read_u32(bytes, (entry_off + 36) as usize).ok_or("truncated stack descriptor")?;
        let ctx_size = read_u32(bytes, (entry_off + 40) as usize).ok_or("truncated context descriptor")?;
        let ctx_rva = read_u32(bytes, (entry_off + 44) as usize).ok_or("truncated context descriptor")?;

        let dir = format!("/threads/{tid}");
        mount.add_file(&format!("{dir}/entry"), entry_off, THREAD_ENTRY_SIZE);
        if ctx_size > 0 {
            mount.add_file(&format!("{dir}/context"), ctx_rva as u64, ctx_size as u64);
        }
        if stack_size > 0 {
            mount.add_file(&format!("{dir}/stack"), stack_rva as u64, stack_size as u64);
        }
    }
    Ok(())
}

/// Parse the module list at `stream_rva` and emit `/modules/<name>/...`.
/// On-disk layout matches the thread list shape:
///
/// * `u32 number_of_modules`
/// * `ModuleEntry[number_of_modules]` (108 bytes each, see [`MODULE_ENTRY_SIZE`])
fn parse_module_list(mount: &mut Mount, bytes: &[u8], stream_rva: u64) -> Result<(), String> {
    let n_modules = read_u32(bytes, stream_rva as usize).ok_or_else(|| "truncated module list header".to_string())?;
    let entries_start = stream_rva + 4;
    let mut name_uses: BTreeMap<String, u32> = BTreeMap::new();
    for i in 0..n_modules as u64 {
        let entry_off = entries_start + i * MODULE_ENTRY_SIZE;
        // ModuleEntry layout:
        //   0  base_of_image u64
        //   8  size_of_image u32
        //  12  checksum u32
        //  16  time_date_stamp u32
        //  20  module_name_rva u32
        //  24  version_info: FixedFileInfo (52 bytes)
        //  76  cv_record: LocationDescriptor32 { data_size u32 (76), rva u32 (80) }
        //  84  misc_record: LocationDescriptor32 { data_size u32 (84), rva u32 (88) }
        //  92  reserved0 u64
        // 100  reserved1 u64
        let name_rva = read_u32(bytes, (entry_off + 20) as usize).ok_or("truncated module entry")?;
        let cv_size = read_u32(bytes, (entry_off + 76) as usize).ok_or("truncated cv descriptor")?;
        let cv_rva = read_u32(bytes, (entry_off + 80) as usize).ok_or("truncated cv descriptor")?;
        let misc_size = read_u32(bytes, (entry_off + 84) as usize).ok_or("truncated misc descriptor")?;
        let misc_rva = read_u32(bytes, (entry_off + 88) as usize).ok_or("truncated misc descriptor")?;

        // The name record at `name_rva` is `u32 byte_length` followed by
        // UTF-16LE bytes. Decode just enough to extract the basename for
        // the path label; expose the raw UTF-16 bytes as a regular file
        // (without the length prefix, which is a separate metadata
        // value -- editing it would corrupt the dump).
        let name_byte_len = read_u32(bytes, name_rva as usize).ok_or("truncated module name length")?;
        let name_bytes_start = name_rva as u64 + 4;
        let name_bytes = bytes
            .get(name_bytes_start as usize..(name_bytes_start as usize + name_byte_len as usize))
            .ok_or_else(|| format!("module name exceeds dump size @ {name_rva:#x}"))?;
        let basename = utf16_basename(name_bytes);
        let counter = name_uses.entry(basename.clone()).or_insert(0);
        let dir_name = if *counter == 0 { basename.clone() } else { format!("{basename}_{counter}") };
        *counter += 1;

        let dir = format!("/modules/{dir_name}");
        mount.add_file(&format!("{dir}/entry"), entry_off, MODULE_ENTRY_SIZE);
        mount.add_file(&format!("{dir}/version_info"), entry_off + 24, FIXED_FILE_INFO_SIZE);
        mount.add_file(&format!("{dir}/name"), name_bytes_start, name_byte_len as u64);
        if cv_size > 0 {
            mount.add_file(&format!("{dir}/cv_record"), cv_rva as u64, cv_size as u64);
        }
        if misc_size > 0 {
            mount.add_file(&format!("{dir}/misc_record"), misc_rva as u64, misc_size as u64);
        }
    }
    Ok(())
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// Decode the basename of a UTF-16LE Windows path. Strips any path
/// separators (`\` or `/`) and any trailing NULs the dump may have
/// included; replaces forward slashes inside the basename so the VFS
/// path stays unambiguous. Returns `module` as a fallback for the
/// (pathological) empty input.
fn utf16_basename(bytes: &[u8]) -> String {
    let mut chars: Vec<u16> = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        chars.push(u16::from_le_bytes([chunk[0], chunk[1]]));
    }
    let trimmed: Vec<u16> = chars.into_iter().take_while(|&c| c != 0).collect();
    let full = String::from_utf16_lossy(&trimmed);
    let basename = full.rsplit(['\\', '/']).next().unwrap_or("module");
    if basename.is_empty() {
        "module".to_string()
    } else {
        basename.replace('/', "_")
    }
}

hxy_plugin_api::handler::export_handler!(Plugin with_types_in hxy_plugin_api::handler);
