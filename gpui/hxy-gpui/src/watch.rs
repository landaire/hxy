//! Filesystem watching for open file tabs, wired to a reload dialog.
//!
//! Wraps the shared `hxy_panels::watch::FileWatcher`, keyed by
//! `gpui::EntityId` (nothing here ever registers a `Vfs` target -- see
//! the module doc below on why). The watcher's background threads
//! cannot touch a `Window`/`App` directly, so [`FileWatch::poll`] is
//! driven from a fixed-cadence `gpui::Timer` loop
//! (`Workspace::spawn_watch_poll`), matching egui's per-frame
//! `drain_file_watch_events`.
//!
//! VFS-entry (mount) watching is out of scope for M3: egui's
//! `refresh_workspace_for_file` re-mounts a workspace tab's nested
//! entries and stages orphan-tab prompts when a reload changes what
//! resolves inside the mount. The GPUI port's `WorkspaceHostPanel` has
//! no equivalent re-mount/orphan machinery yet, so wiring
//! `FileWatcher::watch_vfs` here would have nothing to drive on a
//! fire. Plain on-disk `FilePanel` tabs are the M3 deliverable;
//! `hxy_panels::watch` already supports VFS polling for whenever the
//! nested-dock host grows that plumbing.

use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use gpui::App;
use gpui::Entity;
use gpui::EntityId;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_panels::watch::PollingPrefs;
use hxy_panels::watch::Wake;

use crate::panels::FilePanel;

pub(crate) type FileWatcher = hxy_panels::watch::FileWatcher<EntityId>;
pub(crate) type WatchEvent = hxy_panels::watch::WatchEvent<EntityId>;
pub(crate) type WatchTarget = hxy_panels::watch::WatchTarget<EntityId>;

/// Poll cadence for the reconcile-and-drain loop `Workspace` spawns.
/// egui's equivalent is a per-frame drain (effectively continuous
/// while the window is being interacted with); 500ms keeps the
/// "modified externally" dialog feeling prompt without polling
/// metadata for every open file many times a second.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Owns the shared watcher plus the set of paths currently registered
/// with it, so [`poll`](Self::poll) can diff against the workspace's
/// live open-file paths and issue `watch`/`unwatch` calls itself.
/// Deliberately reconciled from the live set rather than hooked into
/// every tab add/close call site: `Workspace` has several of those
/// (`add_file_panel`, `on_close_tab`, a handful of other close-cascade
/// paths, session-restore resync), and a `HashSet` diff over a
/// handful of paths once per tick is cheap and can't miss one.
pub(crate) struct FileWatch {
    watcher: FileWatcher,
    /// Signalled by the watcher's background threads whenever new
    /// events land. GPUI's context can't be touched from those
    /// threads, so this only proves liveness (drained and discarded
    /// each tick) -- the actual drain runs on the fixed
    /// `POLL_INTERVAL` cadence above regardless. Unlike egui, which
    /// skips idle frames and so genuinely needs the wake to request
    /// one, GPUI's poll loop ticks on its own timer either way.
    wake_rx: mpsc::Receiver<()>,
    watched_paths: HashSet<PathBuf>,
}

impl FileWatch {
    pub(crate) fn new() -> notify::Result<Self> {
        let (tx, wake_rx) = mpsc::channel();
        let wake: Wake = Arc::new(move || {
            let _ = tx.send(());
        });
        let watcher = FileWatcher::with_prefs(wake, PollingPrefs::default())?;
        Ok(Self { watcher, wake_rx, watched_paths: HashSet::new() })
    }

    /// Reconcile watched paths against `live` (every open `FilePanel`'s
    /// on-disk path) and return whatever the watcher has buffered.
    pub(crate) fn poll(&mut self, live: impl Iterator<Item = PathBuf>) -> Vec<WatchEvent> {
        let live: HashSet<PathBuf> = live.collect();
        for stale in self.watched_paths.difference(&live).cloned().collect::<Vec<_>>() {
            self.watcher.unwatch(&stale);
        }
        for fresh in live.difference(&self.watched_paths).cloned().collect::<Vec<_>>() {
            self.watcher.watch(fresh);
        }
        self.watched_paths = live;
        while self.wake_rx.try_recv().is_ok() {}
        self.watcher.drain()
    }

    /// Bump the watcher's snapshot for `path` so it doesn't immediately
    /// re-fire on a change the host just applied (a reload, or an
    /// acknowledged Ignore).
    pub(crate) fn mark_synced(&mut self, path: &Path) {
        self.watcher.mark_synced(path);
    }

    /// The set of paths currently registered with the watcher, for tests
    /// asserting a closed file was reconciled out.
    #[cfg(test)]
    pub(crate) fn watched_paths(&self) -> &HashSet<PathBuf> {
        &self.watched_paths
    }
}

/// Why a watched path changed. A removal always just toasts (there's
/// nothing to reload); a modification stages the reload dialog. See
/// `Workspace::handle_external_change`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExternalChangeKind {
    Modified,
    Removed,
}

/// One choice from the reload-prompt dialog. Routed back into
/// [`apply_reload`] after the user picks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReloadDecision {
    /// Re-read disk bytes; drop the current patch + undo/redo.
    DiscardEdits,
    /// Re-read disk bytes; keep the patch on top of the new base.
    /// Undo/redo are dropped either way (their `old_bytes` reference
    /// the previous base).
    KeepEdits,
    /// Leave the in-memory bytes as they are.
    Ignore,
}

/// One pending reload prompt the workspace is about to show. Only one
/// is queued at a time -- a second file changing before the user
/// responds to this one is dropped (the file stays watched, so a
/// later change re-fires). Mirrors egui's `PendingReloadPrompt`, minus
/// the `Removed` kind: a removal always just toasts (see
/// `Workspace::handle_external_change`) and never reaches here.
pub(crate) struct PendingReloadPrompt {
    pub(crate) file: Entity<FilePanel>,
    pub(crate) display_name: String,
    pub(crate) path: PathBuf,
    pub(crate) has_unsaved: bool,
}

/// Re-read `path` and apply `decision` to `file`'s pane. Returns
/// `false` when the reload can't be applied (read failure); the
/// caller surfaces the diagnostic. `Ignore` is a no-op success that
/// never touches the pane.
pub(crate) fn apply_reload(file: &Entity<FilePanel>, path: &Path, decision: ReloadDecision, cx: &mut App) -> bool {
    if matches!(decision, ReloadDecision::Ignore) {
        return true;
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::error!(?path, %err, "reload: re-read failed");
            return false;
        }
    };
    let fresh: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
    let pane = file.read(cx).pane().clone();
    pane.update(cx, |pane, cx| {
        match decision {
            ReloadDecision::DiscardEdits => pane.editor_mut().swap_source(fresh),
            ReloadDecision::KeepEdits => pane.editor_mut().swap_source_keep_patch(fresh),
            ReloadDecision::Ignore => unreachable!("handled above"),
        }
        cx.notify();
    });
    true
}

#[cfg(test)]
mod tests {
    use gpui::AppContext;
    use gpui::TestAppContext;
    use hxy_core::MemorySource;

    use super::*;
    use crate::panels::FilePanel;

    /// A temp-file modify is picked up by the poll fallback and
    /// surfaces through `poll()` as `Modified`, keyed by the
    /// `EntityId` the caller passed in as the live-path set's owner
    /// (nothing here reads the id -- `poll` only tracks paths -- but
    /// this proves `FileWatch` compiles and drains end to end against
    /// this crate's `EntityId` instantiation).
    #[test]
    fn poll_surfaces_modified_after_external_write_and_reconciles_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watched.bin");
        std::fs::write(&path, b"before").unwrap();

        let mut watch = FileWatch::new().unwrap();
        // First poll: registers the path (nothing to drain yet).
        let events = watch.poll(std::iter::once(path.clone()));
        assert!(events.is_empty());

        std::thread::sleep(Duration::from_millis(2100));
        std::fs::write(&path, b"after-external-change").unwrap();

        let canonical = path.canonicalize().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut saw_modified = false;
        while std::time::Instant::now() < deadline {
            let events = watch.poll(std::iter::once(path.clone()));
            if events.iter().any(|e| matches!(e, WatchEvent::Modified(WatchTarget::Filesystem(p)) if *p == canonical)) {
                saw_modified = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(saw_modified, "expected a Modified event for {canonical:?}");

        // Second reconcile with an empty live set unwatches the path;
        // further external writes must not surface anything.
        let _ = watch.poll(std::iter::empty());
        std::fs::write(&path, b"after-unwatch").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let events = watch.poll(std::iter::empty());
        assert!(events.is_empty(), "unwatched path must not keep firing: {events:?}");
    }

    fn source(bytes: Vec<u8>) -> Arc<dyn HexSource> {
        Arc::new(MemorySource::new(bytes))
    }

    fn build_file_panel(
        cx: &mut TestAppContext,
        bytes: Vec<u8>,
        path: PathBuf,
    ) -> (Entity<FilePanel>, &mut gpui::VisualTestContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(move |window, cx| {
            let panel = cx.new(|cx| FilePanel::new(source(bytes), Some(path), window, cx));
            gpui_component::Root::new(panel, window, cx)
        });
        let root = window.root(cx).unwrap();
        let panel = root.read_with(cx, |root, _| root.view().clone().downcast::<FilePanel>().unwrap());
        let vcx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        vcx.run_until_parked();
        (panel, vcx)
    }

    /// `DiscardEdits` re-reads disk bytes and drops the dirty patch.
    #[gpui::test]
    fn discard_edits_drops_the_patch(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reload.bin");
        std::fs::write(&path, b"aaaa").unwrap();
        let (panel, cx) = build_file_panel(cx, b"aaaa".to_vec(), path.clone());

        panel.update(cx, |panel, cx| {
            panel.pane().update(cx, |pane, cx| {
                pane.editor_mut().splice(0, 1, vec![b'b']).unwrap();
                cx.notify();
            });
        });
        assert!(panel.read_with(cx, |p, cx| p.pane().read(cx).editor().is_dirty()), "edit made the buffer dirty");

        std::fs::write(&path, b"cccc").unwrap();
        let ok = cx.update(|_window, cx| apply_reload(&panel, &path, ReloadDecision::DiscardEdits, cx));
        assert!(ok);

        panel.read_with(cx, |p, cx| {
            let editor = p.pane().read(cx).editor();
            assert!(!editor.is_dirty(), "discard drops the patch");
            let bytes = editor
                .source()
                .read(hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(4)).unwrap())
                .unwrap();
            assert_eq!(&*bytes, b"cccc");
        });
    }

    /// `KeepEdits` re-reads disk bytes but replays the dirty patch on
    /// top of the new base.
    #[gpui::test]
    fn keep_edits_preserves_the_patch(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reload.bin");
        std::fs::write(&path, b"aaaa").unwrap();
        let (panel, cx) = build_file_panel(cx, b"aaaa".to_vec(), path.clone());

        panel.update(cx, |panel, cx| {
            panel.pane().update(cx, |pane, cx| {
                pane.editor_mut().splice(0, 1, vec![b'b']).unwrap();
                cx.notify();
            });
        });
        assert!(panel.read_with(cx, |p, cx| p.pane().read(cx).editor().is_dirty()));

        std::fs::write(&path, b"cccc").unwrap();
        let ok = cx.update(|_window, cx| apply_reload(&panel, &path, ReloadDecision::KeepEdits, cx));
        assert!(ok);

        panel.read_with(cx, |p, cx| {
            let editor = p.pane().read(cx).editor();
            assert!(editor.is_dirty(), "keep-edits preserves the patch");
            let bytes = editor
                .source()
                .read(hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(4)).unwrap())
                .unwrap();
            // The splice replayed on top of the fresh base: byte 0 is
            // the edit ('b'), bytes 1..4 are the new disk content.
            assert_eq!(&*bytes, b"bccc");
        });
    }

    /// `Ignore` never touches the pane and always reports success,
    /// even with no readable file backing the path.
    #[gpui::test]
    fn ignore_leaves_the_pane_untouched(cx: &mut TestAppContext) {
        let path = PathBuf::from("/nonexistent/does-not-matter.bin");
        let (panel, cx) = build_file_panel(cx, b"aaaa".to_vec(), path.clone());

        let ok = cx.update(|_window, cx| apply_reload(&panel, &path, ReloadDecision::Ignore, cx));
        assert!(ok);
        panel.read_with(cx, |p, cx| {
            let bytes = p
                .pane()
                .read(cx)
                .editor()
                .source()
                .read(hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(4)).unwrap())
                .unwrap();
            assert_eq!(&*bytes, b"aaaa");
        });
    }
}
