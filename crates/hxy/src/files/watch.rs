//! egui-side instantiation of the shared file watcher
//! ([`hxy_panels::watch`]): binds its generic VFS-entry `Id` to this
//! app's tab-scoped [`FileId`] and supplies the `Wake` callback as
//! `ctx.request_repaint`.

#![cfg(not(target_arch = "wasm32"))]

use crate::files::FileId;

pub use hxy_panels::watch::PollingPrefs;

pub type WatchEvent = hxy_panels::watch::WatchEvent<FileId>;
pub type WatchTarget = hxy_panels::watch::WatchTarget<FileId>;
pub type FileWatcher = hxy_panels::watch::FileWatcher<FileId>;

/// Construct a watcher with default polling prefs, waking egui via
/// `ctx.request_repaint` whenever the background threads have new
/// events buffered.
pub fn new_watcher(ctx: &egui::Context) -> notify::Result<FileWatcher> {
    new_watcher_with_prefs(ctx, PollingPrefs::default())
}

pub fn new_watcher_with_prefs(ctx: &egui::Context, prefs: PollingPrefs) -> notify::Result<FileWatcher> {
    let ctx = ctx.clone();
    FileWatcher::with_prefs(std::sync::Arc::new(move || ctx.request_repaint()), prefs)
}
