//! egui host wrapper around the shared ImHex-Patterns fetch core
//! (`hxy_templates::patterns_fetch`).
//!
//! The fetch runs on a worker thread so the GUI stays responsive
//! while a few MB of zip downloads. Result is delivered through an
//! `egui_inbox::UiInbox` so the UI re-renders the moment the worker
//! posts a status update -- no polling, no blocking.

#![cfg(not(target_arch = "wasm32"))]

use egui_inbox::UiInbox;
use std::fs;
use std::path::PathBuf;

pub use hxy_templates::patterns_fetch::FetchStatus;
pub use hxy_templates::patterns_fetch::fingerprint_existing;
pub use hxy_templates::patterns_fetch::install_dir;
use hxy_templates::patterns_fetch::run_fetch;

/// Worker handle returned by [`spawn_fetch`]. The host stores it on
/// the app and polls each frame; when the status reaches `Success`
/// or `Failed` the download is over.
pub struct FetchHandle {
    pub inbox: UiInbox<FetchStatus>,
    pub last_status: Option<FetchStatus>,
}

impl FetchHandle {
    /// Drain any new statuses the worker posted. Returns the latest
    /// snapshot so the caller can render progress / error text.
    pub fn pump(&mut self, ctx: &egui::Context) -> Option<&FetchStatus> {
        for s in self.inbox.read(ctx) {
            self.last_status = Some(s);
        }
        self.last_status.as_ref()
    }

    pub fn is_done(&self) -> bool {
        matches!(self.last_status, Some(FetchStatus::Success { .. } | FetchStatus::Failed { .. }))
    }
}

/// Spin off a worker thread that downloads the master tarball and
/// extracts it under `dest`. Returns immediately; the caller polls
/// the returned [`FetchHandle`] for progress and the final hash.
pub fn spawn_fetch(ctx: &egui::Context, dest: PathBuf) -> FetchHandle {
    let (sender, inbox) = UiInbox::channel();
    let ctx_for_thread = ctx.clone();
    crate::background::submit(move || {
        let result = run_fetch(&dest, |status| {
            // Best-effort: if the inbox went away the user closed
            // the app and we don't care about delivery anymore.
            let _ = sender.send(status);
            ctx_for_thread.request_repaint();
        });
        let final_status = match result {
            Ok((sha256_hex, root)) => FetchStatus::Success { sha256_hex, extracted_root: root },
            Err(e) => FetchStatus::Failed { message: e },
        };
        let _ = sender.send(final_status);
        ctx_for_thread.request_repaint();
    });
    FetchHandle { inbox, last_status: None }
}

/// Wrap [`spawn_fetch`] with the standard install path so callers
/// don't have to recompute it.
pub fn spawn_default_fetch(ctx: &egui::Context) -> Option<FetchHandle> {
    let dest = install_dir()?;
    if let Some(parent) = dest.parent() {
        let _ = fs::create_dir_all(parent);
    }
    Some(spawn_fetch(ctx, dest))
}
