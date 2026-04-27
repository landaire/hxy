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
//! Streaming: the mount build walks the dump via small `source.read`
//! calls instead of slurping the whole file into the WASM heap. Peak
//! buffer size during build is the larger of the module list (n × 108
//! bytes) and the memory64 descriptor table (n × 16 bytes) -- typically
//! tens of KB even on multi-GB dumps. After build, the plugin holds
//! only the path-to-(rva, length) map; reads are routed back through
//! `source.read` on demand.
//!
//! Read-only: edits land in the editor's patch overlay but writeback
//! is rejected -- the WIT `source` interface only exposes reads.

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

const MDMP_SIG: [u8; 4] = [b'M', b'D', b'M', b'P'];

const STREAM_THREAD_LIST: u32 = 3;
const STREAM_MODULE_LIST: u32 = 4;
const STREAM_EXCEPTION: u32 = 6;
const STREAM_SYSTEM_INFO: u32 = 7;
const STREAM_MEMORY64_LIST: u32 = 9;

const HEADER_SIZE: u64 = 32;
const DIRECTORY_ENTRY_SIZE: u64 = 12;
const THREAD_ENTRY_SIZE: u64 = 48;
const MODULE_ENTRY_SIZE: u64 = 108;
const FIXED_FILE_INFO_SIZE: u64 = 52;
const MEMORY64_DESCRIPTOR_SIZE: u64 = 16;
/// MEMORY64_LIST stream header preceding the descriptors:
/// `u64 number_of_memory_ranges; u64 base_rva;`.
const MEMORY64_LIST_HEADER_SIZE: u64 = 16;
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

    fn mount_by_token(
        _token: String,
    ) -> Result<
        hxy_plugin_api::handler::exports::hxy::vfs::handler::Mount,
        hxy_plugin_api::handler::exports::hxy::vfs::handler::MountError,
    > {
        Err(hxy_plugin_api::handler::exports::hxy::vfs::handler::MountError {
            message: "udmp does not support token-driven mounts".to_string(),
            retry_label: None,
        })
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

/// A captured memory range. `va_start..va_end` is the process virtual
/// address span; `rva` is where the bytes for that span live in the
/// dump file. Used by [`resolve_va`] to map a thread stack's virtual
/// address (which in full-memory dumps is the only locator we have)
/// back to a file offset.
#[derive(Clone, Copy, Debug)]
struct MemRegion {
    va_start: u64,
    va_end: u64,
    rva: u64,
    length: u64,
}

/// Find the captured region containing `va`, if any, and return the
/// (file offset, available bytes) starting at `va`. `wanted` is the
/// caller's requested length; the available bytes are clamped to
/// what the region actually holds (so a stack VA at the end of a
/// region returns just the tail). Returns `None` when no captured
/// region contains `va`.
fn resolve_va(regions: &[MemRegion], va: u64, wanted: u64) -> Option<(u64, u64)> {
    // Regions are sorted by va_start; the candidate containing `va`
    // is the one with the largest va_start <= va.
    let idx = regions.partition_point(|r| r.va_start <= va).checked_sub(1)?;
    let region = regions.get(idx)?;
    if va >= region.va_end {
        return None;
    }
    let inside = va - region.va_start;
    if inside >= region.length {
        return None;
    }
    let avail = region.length - inside;
    Some((region.rva + inside, wanted.min(avail)))
}

struct Mount {
    files: BTreeMap<String, FileNode>,
    dirs: BTreeMap<String, BTreeSet<String>>,
}

impl Mount {
    fn build() -> Result<Self, String> {
        let total = source::len();
        if total < HEADER_SIZE {
            return Err(format!("dump is {total} bytes; need at least {HEADER_SIZE}"));
        }

        // Header (32 B). All later structural reads are positioned
        // off fields here.
        let header = pull(0, HEADER_SIZE)?;
        if header[0..4] != MDMP_SIG {
            return Err("not a minidump (missing MDMP signature)".to_string());
        }
        let n_streams = read_u32(&header, 8).ok_or("truncated header (n_streams)")?;
        let stream_dir_rva = read_u32(&header, 12).ok_or("truncated header (stream_dir_rva)")? as u64;

        let mut mount = Self { files: BTreeMap::new(), dirs: BTreeMap::new() };
        mount.add_file("/header", 0, HEADER_SIZE);

        // Stream directory: 12 bytes per entry, ~one cache line per
        // dump even at the spec's 64K stream max.
        let dir_total = (n_streams as u64).saturating_mul(DIRECTORY_ENTRY_SIZE);
        let directory = pull(stream_dir_rva, dir_total)?;
        let mut thread_list: Option<u64> = None;
        let mut module_list: Option<u64> = None;
        let mut memory64_list: Option<u64> = None;
        for i in 0..n_streams as usize {
            let off = i * DIRECTORY_ENTRY_SIZE as usize;
            let kind = read_u32(&directory, off).ok_or("truncated directory")?;
            let data_size = read_u32(&directory, off + 4).ok_or("truncated directory")?;
            let rva = read_u32(&directory, off + 8).ok_or("truncated directory")? as u64;
            match kind {
                STREAM_SYSTEM_INFO => {
                    let len = (data_size as u64).max(SYSTEM_INFO_BASE_SIZE);
                    mount.add_file("/streams/system_info", rva, len);
                }
                STREAM_EXCEPTION => {
                    mount.add_file("/streams/exception", rva, data_size as u64);
                }
                STREAM_THREAD_LIST => thread_list = Some(rva),
                STREAM_MODULE_LIST => module_list = Some(rva),
                STREAM_MEMORY64_LIST => memory64_list = Some(rva),
                _ => {}
            }
        }

        // Memory64 list: must come before threads so stack VA->RVA
        // resolution has the region map ready.
        let mem_regions = match memory64_list {
            Some(rva) => parse_memory64_list(&mut mount, rva)?,
            None => Vec::new(),
        };

        if let Some(rva) = thread_list {
            parse_thread_list(&mut mount, rva, &mem_regions)?;
        }
        if let Some(rva) = module_list {
            parse_module_list(&mut mount, rva)?;
        }

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
        Err("udmp mount is read-only".to_string())
    }
}

/// Walk MEMORY64_LIST and emit `/memory/0x<base>` entries.
///
/// On-disk layout:
/// * `u64 number_of_memory_ranges`
/// * `u64 base_rva`  -- file offset where the region data starts
/// * `MemoryDescriptor64[] { u64 start_of_memory_range; u64 data_size; }`
///
/// Each region's data is contiguous starting at `base_rva`, in the
/// same order as the descriptors. The descriptors themselves are
/// pulled in one read so we don't pay a host-boundary trip per
/// region (full-memory dumps routinely have hundreds of regions).
fn parse_memory64_list(mount: &mut Mount, stream_rva: u64) -> Result<Vec<MemRegion>, String> {
    let header = pull(stream_rva, MEMORY64_LIST_HEADER_SIZE)?;
    let n = read_u64(&header, 0).ok_or("truncated memory64 list header")?;
    let base_rva = read_u64(&header, 8).ok_or("truncated memory64 list header")?;

    let descriptors = pull(stream_rva + MEMORY64_LIST_HEADER_SIZE, n.saturating_mul(MEMORY64_DESCRIPTOR_SIZE))?;

    let mut regions: Vec<MemRegion> = Vec::with_capacity(n as usize);
    let mut data_offset = base_rva;
    for i in 0..n as usize {
        let off = i * MEMORY64_DESCRIPTOR_SIZE as usize;
        let va = read_u64(&descriptors, off).ok_or("truncated memory64 descriptor")?;
        let size = read_u64(&descriptors, off + 8).ok_or("truncated memory64 descriptor")?;
        if size == 0 {
            continue;
        }
        regions.push(MemRegion { va_start: va, va_end: va.saturating_add(size), rva: data_offset, length: size });
        mount.add_file(&format!("/memory/0x{va:016X}"), data_offset, size);
        data_offset = data_offset.checked_add(size).ok_or("memory64 data offset overflow")?;
    }
    regions.sort_by_key(|r| r.va_start);
    Ok(regions)
}

/// Parse the thread list at `stream_rva` and emit `/threads/<tid>/...`
/// entries. The on-disk layout is:
///
/// * `u32 number_of_threads`
/// * `ThreadEntry[number_of_threads]` (48 bytes each, see [`THREAD_ENTRY_SIZE`])
///
/// `mem_regions` is the captured-memory map; when a thread's per-entry
/// `Stack.Memory.Rva` is zero (full-memory dumps store stack contents
/// in `MEMORY64_LIST` instead of inline), we resolve the stack's
/// `start_of_memory_range` virtual address against the map to recover
/// the actual file offset.
fn parse_thread_list(mount: &mut Mount, stream_rva: u64, mem_regions: &[MemRegion]) -> Result<(), String> {
    let header = pull(stream_rva, 4)?;
    let n_threads = read_u32(&header, 0).ok_or("truncated thread list header")?;
    if n_threads == 0 {
        return Ok(());
    }

    let entries_total = (n_threads as u64).saturating_mul(THREAD_ENTRY_SIZE);
    let entries = pull(stream_rva + 4, entries_total)?;

    for i in 0..n_threads as u64 {
        let entry_off_local = (i * THREAD_ENTRY_SIZE) as usize;
        let entry_off_file = stream_rva + 4 + i * THREAD_ENTRY_SIZE;

        // ThreadEntry layout (offsets within the entry):
        //   0  thread_id u32
        //   4  suspend_count u32
        //   8  priority_class u32
        //  12  priority u32
        //  16  teb u64
        //  24  stack: MemoryDescriptor { start_of_memory_range u64,
        //                                memory: LocationDescriptor32 { data_size u32 (32), rva u32 (36) } }
        //  40  thread_context: LocationDescriptor32 { data_size u32 (40), rva u32 (44) }
        let tid = read_u32(&entries, entry_off_local).ok_or("truncated thread entry")?;
        let stack_va = read_u64(&entries, entry_off_local + 24).ok_or("truncated stack va")?;
        let stack_size = read_u32(&entries, entry_off_local + 32).ok_or("truncated stack descriptor")?;
        let stack_rva = read_u32(&entries, entry_off_local + 36).ok_or("truncated stack descriptor")?;
        let ctx_size = read_u32(&entries, entry_off_local + 40).ok_or("truncated context descriptor")?;
        let ctx_rva = read_u32(&entries, entry_off_local + 44).ok_or("truncated context descriptor")?;

        let dir = format!("/threads/{tid}");
        mount.add_file(&format!("{dir}/entry"), entry_off_file, THREAD_ENTRY_SIZE);
        if ctx_size > 0 {
            mount.add_file(&format!("{dir}/context"), ctx_rva as u64, ctx_size as u64);
        }
        // Stack: prefer the inline LocationDescriptor when populated;
        // otherwise resolve the start VA against the captured memory
        // regions (full-memory dumps capture stacks in MEMORY64_LIST
        // and leave the inline descriptor's Rva = 0). If neither path
        // yields anything, skip the stack file rather than emit one
        // pointing at offset 0.
        let stack_loc = if stack_rva != 0 && stack_size > 0 {
            Some((stack_rva as u64, stack_size as u64))
        } else if stack_size > 0 {
            resolve_va(mem_regions, stack_va, stack_size as u64)
        } else {
            None
        };
        if let Some((rva, length)) = stack_loc {
            mount.add_file(&format!("{dir}/stack"), rva, length);
        }
    }
    Ok(())
}

/// Parse the module list at `stream_rva` and emit `/modules/<name>/...`.
///
/// On-disk layout:
/// * `u32 number_of_modules`
/// * `ModuleEntry[number_of_modules]` (108 bytes each, see [`MODULE_ENTRY_SIZE`])
///
/// Each module's name lives at a separate `module_name_rva` (a u32
/// length prefix plus UTF-16LE bytes). We pull names individually --
/// they're small and scattered, so a single aggregate read isn't
/// worth the bookkeeping.
fn parse_module_list(mount: &mut Mount, stream_rva: u64) -> Result<(), String> {
    let header = pull(stream_rva, 4)?;
    let n_modules = read_u32(&header, 0).ok_or("truncated module list header")?;
    if n_modules == 0 {
        return Ok(());
    }

    let entries_total = (n_modules as u64).saturating_mul(MODULE_ENTRY_SIZE);
    let entries = pull(stream_rva + 4, entries_total)?;

    let mut name_uses: BTreeMap<String, u32> = BTreeMap::new();
    for i in 0..n_modules as u64 {
        let entry_off_local = (i * MODULE_ENTRY_SIZE) as usize;
        let entry_off_file = stream_rva + 4 + i * MODULE_ENTRY_SIZE;

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
        let name_rva = read_u32(&entries, entry_off_local + 20).ok_or("truncated module entry")? as u64;
        let cv_size = read_u32(&entries, entry_off_local + 76).ok_or("truncated cv descriptor")?;
        let cv_rva = read_u32(&entries, entry_off_local + 80).ok_or("truncated cv descriptor")?;
        let misc_size = read_u32(&entries, entry_off_local + 84).ok_or("truncated misc descriptor")?;
        let misc_rva = read_u32(&entries, entry_off_local + 88).ok_or("truncated misc descriptor")?;

        // Name: u32 byte_length followed by UTF-16LE bytes. Two host
        // calls per module: one for the length, one for the bytes.
        let name_len_buf = pull(name_rva, 4)?;
        let name_byte_len = read_u32(&name_len_buf, 0).ok_or("truncated module name length")? as u64;
        let name_bytes_start = name_rva + 4;
        let name_bytes = pull(name_bytes_start, name_byte_len)?;
        let basename = utf16_basename(&name_bytes);

        let counter = name_uses.entry(basename.clone()).or_insert(0);
        let dir_name = if *counter == 0 { basename.clone() } else { format!("{basename}_{counter}") };
        *counter += 1;

        let dir = format!("/modules/{dir_name}");
        mount.add_file(&format!("{dir}/entry"), entry_off_file, MODULE_ENTRY_SIZE);
        mount.add_file(&format!("{dir}/version_info"), entry_off_file + 24, FIXED_FILE_INFO_SIZE);
        mount.add_file(&format!("{dir}/name"), name_bytes_start, name_byte_len);
        if cv_size > 0 {
            mount.add_file(&format!("{dir}/cv_record"), cv_rva as u64, cv_size as u64);
        }
        if misc_size > 0 {
            mount.add_file(&format!("{dir}/misc_record"), misc_rva as u64, misc_size as u64);
        }
    }
    Ok(())
}

/// Wrap `source.read` so the build path stays readable. Returns an
/// owned `Vec<u8>` of exactly `length` bytes from `offset`, or a
/// formatted error.
fn pull(offset: u64, length: u64) -> Result<Vec<u8>, String> {
    if length == 0 {
        return Ok(Vec::new());
    }
    let buf = source::read(offset, length).map_err(|e| format!("source.read({offset:#x}, {length}): {e}"))?;
    if buf.len() as u64 != length {
        return Err(format!("source.read({offset:#x}, {length}) returned {}", buf.len()));
    }
    Ok(buf)
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let slice = bytes.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_le_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]))
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
