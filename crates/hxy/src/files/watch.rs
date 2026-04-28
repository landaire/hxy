//! Filesystem change tracking for open files.
//!
//! Wraps `notify-debouncer-full` so the host can register a path
//! per open file and drain coalesced "this file changed on disk"
//! events once per frame. A polling worker picks up paths the
//! kernel watcher rejected (network drives, FUSE, etc.) and --
//! when the user opts in -- every watched path so the reload
//! flow keeps working even when notify is silently broken.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use notify::EventKind;
use notify::RecursiveMode;
use notify::event::ModifyKind;
use notify::event::RenameMode;
use notify_debouncer_full::DebounceEventResult;
use notify_debouncer_full::DebouncedEvent;
use notify_debouncer_full::Debouncer;
use notify_debouncer_full::RecommendedCache;
use notify_debouncer_full::new_debouncer;

/// One filesystem change observed for a watched path. Drained
/// per-frame by the host and translated into reload prompts /
/// VFS workspace re-mounts / template re-runs.
#[derive(Clone, Debug)]
pub enum WatchEvent {
    /// File contents changed on disk. The host re-reads the file
    /// (or asks the user, depending on the per-file auto-reload
    /// preference).
    Modified(PathBuf),
    /// File was removed or otherwise made unreachable. The host
    /// surfaces a notice but keeps the in-memory bytes editable.
    Removed(PathBuf),
    /// File was renamed. Both old and new paths are reported so
    /// the host can rewrite the tab's source identity.
    Renamed { from: PathBuf, to: PathBuf },
}

impl WatchEvent {
    pub fn primary_path(&self) -> &Path {
        match self {
            Self::Modified(p) | Self::Removed(p) => p,
            Self::Renamed { from, .. } => from,
        }
    }
}

/// Polling cadence + scope. `interval = None` disables polling
/// entirely (only kernel events fire); `poll_all = true` polls
/// every watched path even when notify accepted it, so users on
/// flaky filesystems can guarantee detection.
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

/// Per-frame snapshot the polling worker compares against to
/// detect drift. Stored both in the worker and in `WatchedEntry`
/// so the host can refresh it after a reload.
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

/// Why a path is enrolled in polling. `Always` means the user
/// asked for it (or `poll_all` is on); `Fallback` means the
/// kernel watcher rejected the path so polling is the only
/// signal we have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PollReason {
    Always,
    Fallback,
}

enum PollCommand {
    Watch { path: PathBuf, reason: PollReason, snapshot: PollSnapshot },
    Unwatch(PathBuf),
    Reset { path: PathBuf, snapshot: PollSnapshot },
    SetInterval(Option<Duration>),
    Shutdown,
}

struct WatchedEntry {
    /// True when notify accepted the path. False means polling
    /// is the sole detector for this entry.
    notify_active: bool,
    /// Tracks whether the worker is also polling this path,
    /// either as a fallback or because `poll_all` is on. Used by
    /// `set_polling` to know what to enrol or drop when the
    /// preference flips.
    polled: bool,
}

/// Cross-platform file change tracker. Owns the kernel-level
/// debouncer, a polling worker, and the per-path bookkeeping
/// needed to keep the two backends in sync.
pub struct FileWatcher {
    debouncer: Debouncer<notify::RecommendedWatcher, RecommendedCache>,
    notify_rx: mpsc::Receiver<DebounceEventResult>,
    poll_rx: mpsc::Receiver<WatchEvent>,
    poll_tx: mpsc::Sender<PollCommand>,
    watched: HashMap<PathBuf, WatchedEntry>,
    polling: PollingPrefs,
}

impl FileWatcher {
    /// Build a watcher with default debounce timeout and the
    /// default polling preferences.
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

        Ok(Self { debouncer, notify_rx, poll_rx, poll_tx, watched: HashMap::new(), polling: prefs })
    }

    /// Replace the polling preferences. Adjusts the worker's
    /// cadence and enrols / unenrols every kernel-covered path
    /// so a `poll_all` toggle takes effect on the very next tick.
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

    /// Register `path` for change tracking. Idempotent. Errors
    /// from `notify::Watcher::watch` flip the path into
    /// poll-only mode rather than propagating.
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

    /// Stop tracking `path`. Best-effort: notify errors during
    /// unwatch (e.g. NotFound) are swallowed.
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

    /// Reset the polled-snapshot for `path` to the current
    /// metadata. Called after the host has accepted a reload so
    /// the worker doesn't immediately re-fire on the same change.
    pub fn mark_synced(&mut self, path: &Path) {
        let canonical = canonicalize(path);
        let Some(entry) = self.watched.get(&canonical) else { return };
        if !entry.polled {
            return;
        }
        let snapshot = snapshot_of(&canonical);
        let _ = self.poll_tx.send(PollCommand::Reset { path: canonical, snapshot });
    }

    /// Drain queued events into a flat list. Notify-side events
    /// come first then the polling worker's; both are filtered
    /// against the live `watched` set so a race between
    /// `unwatch` and a queued event doesn't surface stale
    /// entries.
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
        out.retain(|ev| match ev {
            WatchEvent::Modified(p) | WatchEvent::Removed(p) => self.watched.contains_key(p.as_path()),
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
            paths.iter().map(|p| WatchEvent::Removed(p.clone())).collect()
        }
        EventKind::Create(_)
        | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Any)
        | EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            paths.iter().map(|p| WatchEvent::Modified(p.clone())).collect()
        }
        EventKind::Modify(ModifyKind::Metadata(_) | ModifyKind::Other) => {
            // touch / chmod show up here; treat as a content
            // hint so the host re-checks.
            paths.iter().map(|p| WatchEvent::Modified(p.clone())).collect()
        }
        _ => Vec::new(),
    }
}

fn poll_worker(
    cmd_rx: mpsc::Receiver<PollCommand>,
    event_tx: mpsc::Sender<WatchEvent>,
    ctx: egui::Context,
    initial_interval: Option<Duration>,
) {
    let mut interval = initial_interval;
    let mut tracked: HashMap<PathBuf, (PollReason, PollSnapshot)> = HashMap::new();

    loop {
        // When there's nothing to poll (or polling is off),
        // block until the host sends a command. Otherwise drain
        // pending commands non-blockingly so the next tick uses
        // the latest watch set.
        let first: Option<PollCommand> = if tracked.is_empty() || interval.is_none() {
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
                PollCommand::SetInterval(new) => interval = new,
                PollCommand::Shutdown => return,
            }
        }
        if tracked.is_empty() || interval.is_none() {
            continue;
        }

        std::thread::sleep(interval.expect("checked above"));

        let mut emitted = false;
        for (path, (_reason, last)) in tracked.iter_mut() {
            let current = snapshot_of(path);
            if current == *last {
                continue;
            }
            let event = if !current.exists && last.exists {
                WatchEvent::Removed(path.clone())
            } else {
                WatchEvent::Modified(path.clone())
            };
            *last = current;
            if event_tx.send(event).is_err() {
                return;
            }
            emitted = true;
        }
        if emitted {
            ctx.request_repaint();
        }
    }
}
