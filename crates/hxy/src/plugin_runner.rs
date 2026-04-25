//! Background-thread driver for plugin operations.
//!
//! Plugin invocations call into wasmtime which calls into the
//! plugin's wasm code which may block on TCP / UDP / disk I/O. Doing
//! that on the UI thread freezes the whole frame loop. This module
//! shuttles those calls to a fresh OS thread per operation, queues
//! the result on an mpsc channel, and lets the egui app drain
//! completed operations once a frame.
//!
//! Three operation kinds are supported (mirroring
//! [`PluginHandler`](hxy_plugin_host::PluginHandler)'s public surface):
//!
//! - [`spawn_invoke`] -- runs `plugin.invoke_command(id)`
//! - [`spawn_respond`] -- runs `plugin.respond_to_prompt(id, answer)`
//! - [`spawn_mount_by_token`] -- runs `plugin.mount_by_token(token)`
//!
//! Mount-internal operations (`read_dir` / `metadata` / `read_file`
//! on a `MountedVfs`) still run synchronously on the UI thread today.
//! Those are called from the egui rendering loop and would need a
//! larger restructure (per-mount worker thread + cached results) to
//! offload. Tracked in xbox-neighborhood TODO list.
//!
//! Every spawned op also calls back into the app's console log via
//! the [`Logger`] callback at start and finish so the user can see
//! what the plugin is doing without flipping to terminal-side
//! tracing output.

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use hxy_plugin_host::InvokeOutcome;
use hxy_plugin_host::PluginHandler;
use hxy_vfs::MountedVfs;

use crate::app::ConsoleSeverity;

/// Sink for activity-log entries. Implemented by `HxyApp` so the
/// runner can write into the Console tab without taking a circular
/// dependency on the whole app type.
pub trait Logger {
    fn log(&mut self, severity: ConsoleSeverity, context: String, message: String);
}

/// One outstanding plugin operation. Held on the app side; once a
/// frame the app polls its `rx` and dispatches the result.
pub struct PendingOp {
    pub plugin_name: String,
    /// Short label for log entries, e.g. `"invoke connect"` or
    /// `"mount xbox.local:730"`.
    pub label: String,
    pub started: Instant,
    pub kind: PendingKind,
}

pub enum PendingKind {
    Invoke {
        rx: mpsc::Receiver<Option<InvokeOutcome>>,
        plugin: Arc<PluginHandler>,
        command_id: String,
    },
    Respond {
        rx: mpsc::Receiver<Option<InvokeOutcome>>,
        plugin: Arc<PluginHandler>,
        command_id: String,
        answer: String,
    },
    MountByToken {
        rx: mpsc::Receiver<Result<MountedVfs, String>>,
        plugin: Arc<PluginHandler>,
        token: String,
        title: String,
    },
}

/// Output of polling a [`PendingOp`].
pub enum DrainResult {
    /// Operation hasn't completed yet -- leave it in the queue.
    Pending,
    /// User-driven invoke finished. The dispatcher should route
    /// the outcome through the palette.
    InvokeReady {
        plugin: Arc<PluginHandler>,
        command_id: String,
        outcome: Option<InvokeOutcome>,
    },
    /// User-driven respond finished. Same dispatch as `InvokeReady`.
    RespondReady {
        plugin: Arc<PluginHandler>,
        command_id: String,
        outcome: Option<InvokeOutcome>,
    },
    /// `mount-by-token` finished. The dispatcher should open a new
    /// tab backed by `mount` (or surface the error in the console).
    MountReady {
        plugin: Arc<PluginHandler>,
        token: String,
        title: String,
        result: Result<MountedVfs, String>,
    },
}

impl PendingOp {
    /// Try to take the result without blocking. Returns
    /// [`DrainResult::Pending`] if the worker thread hasn't
    /// completed yet -- caller leaves the op queued and tries
    /// again next frame.
    pub fn try_take(self) -> Result<DrainResult, Self> {
        match self.kind {
            PendingKind::Invoke { rx, plugin, command_id } => match rx.try_recv() {
                Ok(outcome) => Ok(DrainResult::InvokeReady { plugin, command_id, outcome }),
                Err(mpsc::TryRecvError::Empty) => Err(PendingOp {
                    plugin_name: self.plugin_name,
                    label: self.label,
                    started: self.started,
                    kind: PendingKind::Invoke { rx, plugin, command_id },
                }),
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Worker thread panicked -- treat as `None` so
                    // the dispatcher closes the palette without
                    // mutating UI state mid-flight.
                    Ok(DrainResult::InvokeReady { plugin, command_id, outcome: None })
                }
            },
            PendingKind::Respond { rx, plugin, command_id, answer: _ } => match rx.try_recv() {
                Ok(outcome) => Ok(DrainResult::RespondReady { plugin, command_id, outcome }),
                Err(mpsc::TryRecvError::Empty) => Err(PendingOp {
                    plugin_name: self.plugin_name,
                    label: self.label,
                    started: self.started,
                    kind: PendingKind::Respond { rx, plugin, command_id, answer: String::new() },
                }),
                Err(mpsc::TryRecvError::Disconnected) => {
                    Ok(DrainResult::RespondReady { plugin, command_id, outcome: None })
                }
            },
            PendingKind::MountByToken { rx, plugin, token, title } => match rx.try_recv() {
                Ok(result) => Ok(DrainResult::MountReady { plugin, token, title, result }),
                Err(mpsc::TryRecvError::Empty) => Err(PendingOp {
                    plugin_name: self.plugin_name,
                    label: self.label,
                    started: self.started,
                    kind: PendingKind::MountByToken { rx, plugin, token, title },
                }),
                Err(mpsc::TryRecvError::Disconnected) => Ok(DrainResult::MountReady {
                    plugin,
                    token,
                    title,
                    result: Err("plugin worker thread panicked".to_owned()),
                }),
            },
        }
    }
}

/// Spawn a worker thread to run `plugin.invoke_command(command_id)`
/// and register the resulting [`PendingOp`] on `pending`. Logs the
/// "started" entry via `logger`.
pub fn spawn_invoke<L: Logger>(
    pending: &mut Vec<PendingOp>,
    logger: &mut L,
    repaint: egui::Context,
    plugin: Arc<PluginHandler>,
    plugin_name: String,
    command_id: String,
) {
    let label = format!("invoke {command_id}");
    logger.log(
        ConsoleSeverity::Info,
        format!("plugin/{plugin_name}"),
        format!("{label} started"),
    );

    let (tx, rx) = mpsc::sync_channel(1);
    let plugin_clone = plugin.clone();
    let command_id_clone = command_id.clone();
    thread::spawn(move || {
        let outcome = plugin_clone.invoke_command(&command_id_clone);
        // Channel send failure means the UI is gone -- nothing
        // useful to do here either way.
        let _ = tx.send(outcome);
        repaint.request_repaint();
    });

    pending.push(PendingOp {
        plugin_name,
        label,
        started: Instant::now(),
        kind: PendingKind::Invoke { rx, plugin, command_id },
    });
}

/// Spawn a worker thread to run `plugin.respond_to_prompt(command_id, answer)`.
pub fn spawn_respond<L: Logger>(
    pending: &mut Vec<PendingOp>,
    logger: &mut L,
    repaint: egui::Context,
    plugin: Arc<PluginHandler>,
    plugin_name: String,
    command_id: String,
    answer: String,
) {
    let label = format!("respond {command_id}");
    logger.log(
        ConsoleSeverity::Info,
        format!("plugin/{plugin_name}"),
        format!("{label} started (answer = {answer:?})"),
    );

    let (tx, rx) = mpsc::sync_channel(1);
    let plugin_clone = plugin.clone();
    let command_id_clone = command_id.clone();
    let answer_clone = answer.clone();
    thread::spawn(move || {
        let outcome = plugin_clone.respond_to_prompt(&command_id_clone, &answer_clone);
        let _ = tx.send(outcome);
        repaint.request_repaint();
    });

    pending.push(PendingOp {
        plugin_name,
        label,
        started: Instant::now(),
        kind: PendingKind::Respond { rx, plugin, command_id, answer },
    });
}

/// Spawn a worker thread to run `plugin.mount_by_token(token)` --
/// the slow part of opening a console tab.
pub fn spawn_mount_by_token<L: Logger>(
    pending: &mut Vec<PendingOp>,
    logger: &mut L,
    repaint: egui::Context,
    plugin: Arc<PluginHandler>,
    plugin_name: String,
    token: String,
    title: String,
) {
    let label = format!("mount {token}");
    logger.log(
        ConsoleSeverity::Info,
        format!("plugin/{plugin_name}"),
        format!("{label} started"),
    );

    let (tx, rx) = mpsc::sync_channel(1);
    let plugin_clone = plugin.clone();
    let token_clone = token.clone();
    thread::spawn(move || {
        let result = plugin_clone
            .mount_by_token(&token_clone)
            .map_err(|e| e.to_string());
        let _ = tx.send(result);
        repaint.request_repaint();
    });

    pending.push(PendingOp {
        plugin_name,
        label,
        started: Instant::now(),
        kind: PendingKind::MountByToken { rx, plugin, token, title },
    });
}

/// Format a "completed in N ms" log entry. Centralised so log
/// formatting stays consistent across the three op kinds.
pub fn log_completion<L: Logger>(
    logger: &mut L,
    plugin_name: &str,
    label: &str,
    started: Instant,
    severity: ConsoleSeverity,
    detail: &str,
) {
    let elapsed = started.elapsed();
    logger.log(
        severity,
        format!("plugin/{plugin_name}"),
        format!("{label} {detail} ({})", format_elapsed(elapsed)),
    );
}

fn format_elapsed(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.2} s", d.as_secs_f64())
    }
}
