//! Change tracking for open files.
//!
//! Wraps `notify-debouncer-full` for kernel-level filesystem
//! events plus a polling worker that handles two extra cases:
//! filesystem paths the kernel watcher rejected (and the
//! optional opt-in `poll_all` mode) and -- new in this module --
//! VFS-entry tabs that have no filesystem path at all (xbox
//! memory, plugin mounts, etc.). VFS detection works by
//! re-reading a small sample of byte ranges through the entry's
//! streaming source on every poll tick and emitting a
//! `Modified` event when any range's BLAKE3 fingerprint drifts.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;

use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;
use notify::EventKind;
use notify::RecursiveMode;
use notify::event::ModifyKind;
use notify::event::RenameMode;
use notify_debouncer_full::DebounceEventResult;
use notify_debouncer_full::DebouncedEvent;
use notify_debouncer_full::Debouncer;
use notify_debouncer_full::RecommendedCache;
use notify_debouncer_full::new_debouncer;
use suture::metadata::HashAlgorithm;

use crate::files::FileId;

/// One change observed for a watched target. Drained per-frame
/// and translated into reload prompts / VFS workspace re-mounts
/// / template re-runs.
#[derive(Clone, Debug)]
pub enum WatchEvent {
    Modified(WatchTarget),
    Removed(WatchTarget),
    /// Filesystem-only: a rename from one disk path to another.
    /// VFS entries don't have a rename concept; renames inside
    /// a mount surface as a Removed for the old name plus a
    /// Modified for the parent file (which the workspace
    /// re-mount path picks up).
    Renamed { from: PathBuf, to: PathBuf },
}

/// What the watcher reported a change against. Filesystem
/// paths come from notify or the metadata-polling worker; VFS
/// keys come from the sample-hash worker that re-reads byte
/// ranges through a streaming source.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum WatchTarget {
    Filesystem(PathBuf),
    Vfs(FileId),
}

impl WatchEvent {
    /// Best-effort path string for diagnostics and console
    /// labels. VFS targets don't have a real path -- we render
    /// `vfs://<file-id>` so the log line is grep-able.
    pub fn display(&self) -> String {
        match self {
            Self::Modified(t) | Self::Removed(t) => target_display(t),
            Self::Renamed { from, to } => format!("{} -> {}", from.display(), to.display()),
        }
    }
}

fn target_display(target: &WatchTarget) -> String {
    match target {
        WatchTarget::Filesystem(p) => p.display().to_string(),
        WatchTarget::Vfs(id) => format!("vfs://{}", id.get()),
    }
}

/// Polling cadence + scope. `interval = None` disables polling
/// entirely (only kernel events fire); `poll_all = true` polls
/// every watched filesystem path even when notify accepted it.
/// VFS-entry polling is always on when `interval` is `Some(_)` --
/// without polling there is no detection signal at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PollingPrefs {
    pub interval: Option<Duration>,
    pub poll_all: bool,
}

impl PollingPrefs {
    pub const MIN_INTERVAL: Duration = Duration::from_millis(250);
    pub const MAX_INTERVAL: Duration = Duration::from_secs(600);
    pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(2);
}

impl Default for PollingPrefs {
    fn default() -> Self {
        Self { interval: Some(Self::DEFAULT_INTERVAL), poll_all: false }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PollSnapshot {
    size: u64,
    mtime_secs: i64,
    mtime_nanos: u32,
    exists: bool,
}

impl PollSnapshot {
    fn from_metadata(meta: &std::fs::Metadata) -> Self {
        let (secs, nanos) = match meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()) {
            Some(d) => (d.as_secs() as i64, d.subsec_nanos()),
            None => (0, 0),
        };
        Self { size: meta.len(), mtime_secs: secs, mtime_nanos: nanos, exists: true }
    }

    fn missing() -> Self {
        Self { size: 0, mtime_secs: 0, mtime_nanos: 0, exists: false }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PollReason {
    Always,
    Fallback,
}

/// Sample plan for a VFS entry: which ranges to read through
/// the streaming source on each poll tick. Sized to favour
/// catching contents changes near the start / end of the entry
/// (typical for headers + appended writes) plus a middle
/// sample for in-place rewrites.
const VFS_SAMPLE_HEAD: u64 = 4096;
const VFS_SAMPLE_TAIL: u64 = 4096;
const VFS_SAMPLE_MID: u64 = 4096;

fn vfs_sample_ranges(len: u64) -> Vec<Range<u64>> {
    if len == 0 {
        return Vec::new();
    }
    if len <= VFS_SAMPLE_HEAD + VFS_SAMPLE_TAIL + VFS_SAMPLE_MID {
        return vec![0..len];
    }
    let head = 0..VFS_SAMPLE_HEAD;
    let tail_start = len.saturating_sub(VFS_SAMPLE_TAIL);
    let tail = tail_start..len;
    let mid_center = len / 2;
    let mid_start = mid_center.saturating_sub(VFS_SAMPLE_MID / 2);
    let mid_end = (mid_start + VFS_SAMPLE_MID).min(tail_start);
    if mid_end <= mid_start {
        return vec![head, tail];
    }
    vec![head, mid_start..mid_end, tail]
}

#[derive(Clone)]
struct VfsFingerprint {
    /// Total length captured at registration time. A length
    /// drift alone is enough to fire `Modified` -- the per-
    /// range hashes are only checked when the length matches.
    len: u64,
    /// One fingerprint per sample range, in the same order as
    /// [`vfs_sample_ranges`].
    sample_hashes: Vec<[u8; 32]>,
}

enum PollCommand {
    Watch { path: PathBuf, reason: PollReason, snapshot: PollSnapshot },
    Unwatch(PathBuf),
    Reset { path: PathBuf, snapshot: PollSnapshot },
    WatchVfs { id: FileId, source: Arc<dyn HexSource>, fingerprint: VfsFingerprint },
    UnwatchVfs(FileId),
    ResetVfs { id: FileId, fingerprint: VfsFingerprint },
    SetInterval(Option<Duration>),
    Shutdown,
}

struct WatchedEntry {
    notify_active: bool,
    polled: bool,
}

/// Cross-platform change tracker. Owns the kernel-level
/// debouncer for filesystem paths, a polling worker that
/// covers both filesystem fallback paths and every registered
/// VFS-entry tab, and the per-target bookkeeping needed to
/// keep the two backends in sync.
pub struct FileWatcher {
    debouncer: Debouncer<notify::RecommendedWatcher, RecommendedCache>,
    notify_rx: mpsc::Receiver<DebounceEventResult>,
    poll_rx: mpsc::Receiver<WatchEvent>,
    poll_tx: mpsc::Sender<PollCommand>,
    watched: HashMap<PathBuf, WatchedEntry>,
    /// VFS entries currently being polled. Stored so `unwatch_vfs`
    /// is idempotent and so events arriving after unwatch can be
    /// suppressed in `drain`.
    watched_vfs: Mutex<HashMap<FileId, ()>>,
    polling: PollingPrefs,
}

impl FileWatcher {
    pub fn new(ctx: &egui::Context) -> notify::Result<Self> {
        Self::with_prefs(ctx, PollingPrefs::default())
    }

    pub fn with_prefs(ctx: &egui::Context, prefs: PollingPrefs) -> notify::Result<Self> {
        let (notify_tx, notify_rx) = mpsc::channel::<DebounceEventResult>();
        let ctx_for_notify = ctx.clone();
        let debouncer = new_debouncer(Duration::from_millis(500), None, move |res| {
            let _ = notify_tx.send(res);
            ctx_for_notify.request_repaint();
        })?;

        let (poll_tx, poll_cmd_rx) = mpsc::channel::<PollCommand>();
        let (poll_event_tx, poll_rx) = mpsc::channel::<WatchEvent>();
        let ctx_for_poll = ctx.clone();
        let initial_interval = prefs.interval;
        std::thread::Builder::new()
            .name("hxy-file-poll".into())
            .spawn(move || poll_worker(poll_cmd_rx, poll_event_tx, ctx_for_poll, initial_interval))
            .expect("spawn file poll worker");

        Ok(Self {
            debouncer,
            notify_rx,
            poll_rx,
            poll_tx,
            watched: HashMap::new(),
            watched_vfs: Mutex::new(HashMap::new()),
            polling: prefs,
        })
    }

    pub fn set_polling(&mut self, prefs: PollingPrefs) {
        if prefs == self.polling {
            return;
        }
        let prev_poll_all = self.polling.poll_all;
        self.polling = prefs;
        let _ = self.poll_tx.send(PollCommand::SetInterval(prefs.interval));
        if prefs.poll_all == prev_poll_all {
            return;
        }
        let paths: Vec<PathBuf> = self.watched.keys().cloned().collect();
        for path in paths {
            let entry = self.watched.get_mut(&path).expect("just collected");
            if !entry.notify_active {
                continue;
            }
            if prefs.poll_all && !entry.polled {
                let snapshot = snapshot_of(&path);
                let _ = self.poll_tx.send(PollCommand::Watch { path, reason: PollReason::Always, snapshot });
                entry.polled = true;
            } else if !prefs.poll_all && entry.polled {
                let _ = self.poll_tx.send(PollCommand::Unwatch(path));
                entry.polled = false;
            }
        }
    }

    pub fn polling(&self) -> PollingPrefs {
        self.polling
    }

    pub fn watch(&mut self, path: PathBuf) {
        let canonical = canonicalize(&path);
        if self.watched.contains_key(&canonical) {
            return;
        }
        let notify_active = match self.debouncer.watch(&canonical, RecursiveMode::NonRecursive) {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!(error = %e, path = %canonical.display(), "notify rejected path; polling instead");
                false
            }
        };
        let needs_poll = !notify_active || self.polling.poll_all;
        if needs_poll {
            let reason = if notify_active { PollReason::Always } else { PollReason::Fallback };
            let snapshot = snapshot_of(&canonical);
            let _ = self.poll_tx.send(PollCommand::Watch { path: canonical.clone(), reason, snapshot });
        }
        self.watched.insert(canonical, WatchedEntry { notify_active, polled: needs_poll });
    }

    pub fn unwatch(&mut self, path: &Path) {
        let canonical = canonicalize(path);
        let Some(entry) = self.watched.remove(&canonical) else { return };
        if entry.notify_active {
            let _ = self.debouncer.unwatch(&canonical);
        }
        if entry.polled {
            let _ = self.poll_tx.send(PollCommand::Unwatch(canonical));
        }
    }

    pub fn mark_synced(&mut self, path: &Path) {
        let canonical = canonicalize(path);
        let Some(entry) = self.watched.get(&canonical) else { return };
        if !entry.polled {
            return;
        }
        let snapshot = snapshot_of(&canonical);
        let _ = self.poll_tx.send(PollCommand::Reset { path: canonical, snapshot });
    }

    /// Register a VFS-entry tab for sample-hash polling. The
    /// entry's bytes are read through `source` on every poll
    /// tick at the offsets returned by [`vfs_sample_ranges`];
    /// drift fires `WatchEvent::Modified(WatchTarget::Vfs(id))`.
    /// The initial fingerprint is computed synchronously so the
    /// caller pays the upfront cost in the open path rather
    /// than the first poll.
    pub fn watch_vfs(&mut self, id: FileId, source: Arc<dyn HexSource>) {
        let fingerprint = match vfs_fingerprint(&*source) {
            Ok(f) => f,
            Err(e) => {
                tracing::debug!(error = %e, file = id.get(), "vfs fingerprint failed; not polling this entry");
                return;
            }
        };
        self.watched_vfs.lock().expect("watched_vfs mutex").insert(id, ());
        let _ = self.poll_tx.send(PollCommand::WatchVfs { id, source, fingerprint });
    }

    pub fn unwatch_vfs(&mut self, id: FileId) {
        if self.watched_vfs.lock().expect("watched_vfs mutex").remove(&id).is_none() {
            return;
        }
        let _ = self.poll_tx.send(PollCommand::UnwatchVfs(id));
    }

    /// Re-snapshot a VFS entry's fingerprint after the host
    /// applied a reload, so the next tick doesn't immediately
    /// re-fire on the same change.
    pub fn mark_vfs_synced(&mut self, id: FileId, source: Arc<dyn HexSource>) {
        if !self.watched_vfs.lock().expect("watched_vfs mutex").contains_key(&id) {
            return;
        }
        let fingerprint = match vfs_fingerprint(&*source) {
            Ok(f) => f,
            Err(e) => {
                tracing::debug!(error = %e, file = id.get(), "refresh vfs fingerprint");
                return;
            }
        };
        let _ = self.poll_tx.send(PollCommand::ResetVfs { id, fingerprint });
    }

    pub fn drain(&mut self) -> Vec<WatchEvent> {
        let mut out: Vec<WatchEvent> = Vec::new();
        loop {
            match self.notify_rx.try_recv() {
                Ok(Ok(events)) => {
                    for ev in events {
                        out.extend(translate_notify_event(&ev));
                    }
                }
                Ok(Err(errs)) => {
                    for err in errs {
                        tracing::debug!(error = %err, "notify watcher reported error");
                    }
                }
                Err(_) => break,
            }
        }
        while let Ok(ev) = self.poll_rx.try_recv() {
            out.push(ev);
        }
        let watched_vfs = self.watched_vfs.lock().expect("watched_vfs mutex");
        out.retain(|ev| match ev {
            WatchEvent::Modified(WatchTarget::Filesystem(p))
            | WatchEvent::Removed(WatchTarget::Filesystem(p)) => self.watched.contains_key(p.as_path()),
            WatchEvent::Modified(WatchTarget::Vfs(id)) | WatchEvent::Removed(WatchTarget::Vfs(id)) => {
                watched_vfs.contains_key(id)
            }
            WatchEvent::Renamed { from, to } => {
                self.watched.contains_key(from.as_path()) || self.watched.contains_key(to.as_path())
            }
        });
        out
    }
}

impl Drop for FileWatcher {
    fn drop(&mut self) {
        let _ = self.poll_tx.send(PollCommand::Shutdown);
    }
}

fn canonicalize(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn snapshot_of(path: &Path) -> PollSnapshot {
    match std::fs::metadata(path) {
        Ok(meta) => PollSnapshot::from_metadata(&meta),
        Err(_) => PollSnapshot::missing(),
    }
}

fn translate_notify_event(event: &DebouncedEvent) -> Vec<WatchEvent> {
    let kind = event.kind;
    let paths = &event.paths;
    if paths.is_empty() {
        return Vec::new();
    }
    match kind {
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if paths.len() == 2 => {
            vec![WatchEvent::Renamed { from: paths[0].clone(), to: paths[1].clone() }]
        }
        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            paths.iter().map(|p| WatchEvent::Removed(WatchTarget::Filesystem(p.clone()))).collect()
        }
        EventKind::Create(_)
        | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Any)
        | EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            paths.iter().map(|p| WatchEvent::Modified(WatchTarget::Filesystem(p.clone()))).collect()
        }
        // Metadata-only events (chmod, atime updates) and the
        // catch-all Other variant are dropped: the user
        // doesn't think of those as "the file changed", and
        // macOS fsevent fires Modify::Metadata when our own
        // process opens the file for read. The polling worker
        // still catches actual content drift via
        // (size, mtime) comparison, so a real edit that
        // bypasses the kernel watcher doesn't go unnoticed.
        EventKind::Modify(ModifyKind::Metadata(_) | ModifyKind::Other) => Vec::new(),
        _ => Vec::new(),
    }
}

/// Read each sample range out of `source` and hash it with
/// BLAKE3. Returns the per-range fingerprint so the polling
/// worker can detect drift on the next tick without holding
/// the bytes themselves.
fn vfs_fingerprint(source: &dyn HexSource) -> Result<VfsFingerprint, String> {
    let len = source.len().get();
    let ranges = vfs_sample_ranges(len);
    let mut sample_hashes = Vec::with_capacity(ranges.len());
    for r in ranges {
        let range = ByteRange::new(ByteOffset::new(r.start), ByteOffset::new(r.end))
            .map_err(|e| format!("range {}..{}: {e}", r.start, r.end))?;
        let bytes = source.read(range).map_err(|e| format!("read {}..{}: {e}", r.start, r.end))?;
        let digest = HashAlgorithm::Blake3.compute(&bytes);
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&digest[..32.min(digest.len())]);
        sample_hashes.push(arr);
    }
    Ok(VfsFingerprint { len, sample_hashes })
}

struct VfsTracked {
    source: Arc<dyn HexSource>,
    fingerprint: VfsFingerprint,
}

fn poll_worker(
    cmd_rx: mpsc::Receiver<PollCommand>,
    event_tx: mpsc::Sender<WatchEvent>,
    ctx: egui::Context,
    initial_interval: Option<Duration>,
) {
    let mut interval = initial_interval;
    let mut tracked: HashMap<PathBuf, (PollReason, PollSnapshot)> = HashMap::new();
    let mut tracked_vfs: HashMap<FileId, VfsTracked> = HashMap::new();

    loop {
        let idle_before = tracked.is_empty() && tracked_vfs.is_empty();
        let first: Option<PollCommand> = if idle_before || interval.is_none() {
            match cmd_rx.recv() {
                Ok(cmd) => Some(cmd),
                Err(_) => return,
            }
        } else {
            None
        };
        let mut commands: Vec<PollCommand> = first.into_iter().collect();
        loop {
            match cmd_rx.try_recv() {
                Ok(cmd) => commands.push(cmd),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }
        for cmd in commands {
            match cmd {
                PollCommand::Watch { path, reason, snapshot } => {
                    tracked.insert(path, (reason, snapshot));
                }
                PollCommand::Unwatch(path) => {
                    tracked.remove(&path);
                }
                PollCommand::Reset { path, snapshot } => {
                    if let Some(slot) = tracked.get_mut(&path) {
                        slot.1 = snapshot;
                    }
                }
                PollCommand::WatchVfs { id, source, fingerprint } => {
                    tracked_vfs.insert(id, VfsTracked { source, fingerprint });
                }
                PollCommand::UnwatchVfs(id) => {
                    tracked_vfs.remove(&id);
                }
                PollCommand::ResetVfs { id, fingerprint } => {
                    if let Some(slot) = tracked_vfs.get_mut(&id) {
                        slot.fingerprint = fingerprint;
                    }
                }
                PollCommand::SetInterval(new) => interval = new,
                PollCommand::Shutdown => return,
            }
        }
        if (tracked.is_empty() && tracked_vfs.is_empty()) || interval.is_none() {
            continue;
        }
        std::thread::sleep(interval.expect("checked above"));

        let mut emitted = false;

        // Filesystem-fallback / poll-all metadata checks.
        for (path, (_reason, last)) in tracked.iter_mut() {
            let current = snapshot_of(path);
            if current == *last {
                continue;
            }
            let event = if !current.exists && last.exists {
                WatchEvent::Removed(WatchTarget::Filesystem(path.clone()))
            } else {
                WatchEvent::Modified(WatchTarget::Filesystem(path.clone()))
            };
            *last = current;
            if event_tx.send(event).is_err() {
                return;
            }
            emitted = true;
        }

        // VFS sample-hash checks. Failing reads (mount torn
        // down, entry vanished, plugin offline) surface as a
        // Removed; differing hashes surface as Modified. The
        // fingerprint is updated in either case so we don't
        // re-emit on the next tick.
        for (id, tracked) in tracked_vfs.iter_mut() {
            let current = match vfs_fingerprint(&*tracked.source) {
                Ok(f) => f,
                Err(e) => {
                    tracing::debug!(error = %e, file = id.get(), "vfs poll read failed");
                    if event_tx.send(WatchEvent::Removed(WatchTarget::Vfs(*id))).is_err() {
                        return;
                    }
                    emitted = true;
                    continue;
                }
            };
            if current.len != tracked.fingerprint.len || current.sample_hashes != tracked.fingerprint.sample_hashes {
                if event_tx.send(WatchEvent::Modified(WatchTarget::Vfs(*id))).is_err() {
                    return;
                }
                tracked.fingerprint = current;
                emitted = true;
            }
        }
        if emitted {
            ctx.request_repaint();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hxy_core::MemorySource;

    #[test]
    fn vfs_sample_ranges_short_is_single_full_read() {
        let r = vfs_sample_ranges(1024);
        assert_eq!(r, vec![0..1024]);
    }

    #[test]
    fn vfs_sample_ranges_long_picks_three_windows() {
        let len = 10 * 1024 * 1024;
        let r = vfs_sample_ranges(len);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].start, 0);
        assert_eq!(r[0].end, VFS_SAMPLE_HEAD);
        assert_eq!(r[2].end, len);
        assert!(r[1].start > r[0].end && r[1].end < r[2].start);
    }

    #[test]
    fn fingerprint_drifts_when_bytes_change() {
        let src1: Arc<dyn HexSource> = Arc::new(MemorySource::new(vec![0u8; 100]));
        let src2: Arc<dyn HexSource> = Arc::new(MemorySource::new(vec![1u8; 100]));
        let fp1 = vfs_fingerprint(&*src1).unwrap();
        let fp2 = vfs_fingerprint(&*src2).unwrap();
        assert_ne!(fp1.sample_hashes, fp2.sample_hashes);
    }

    #[test]
    fn fingerprint_stable_for_identical_sources() {
        let bytes: Vec<u8> = (0..200u8).collect();
        let a: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes.clone()));
        let b: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
        assert_eq!(vfs_fingerprint(&*a).unwrap().sample_hashes, vfs_fingerprint(&*b).unwrap().sample_hashes);
    }
}
