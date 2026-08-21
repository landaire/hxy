//! Shared console-log types and the app-global read buffer backing the
//! Console tab.
//!
//! Mirrors the egui app's `ConsoleEntry` / `ConsoleSeverity`
//! (`crates/hxy/src/app/mod.rs`) within the gpui shell: the plugin
//! runner (`crate::plugins`) and the template runner
//! (`crate::templates`) both feed it, and the [`ConsolePanel`]
//! (`crate::panels::console_view`) renders it. The egui frontend keeps
//! its own copy of these types; this unifies only the gpui side.
//!
//! [`Workspace`](crate::workspace::Workspace) owns the authoritative
//! capacity-bounded buffer; each `console_log` republishes it as
//! [`ConsoleLogGlobal`] so the panel can read it without a handle back
//! to the workspace.

use gpui::Global;

/// Ring capacity for the console buffer, matching the egui app's
/// `HxyApp::CONSOLE_CAPACITY`. Older entries are evicted first so a
/// long session's log never grows without bound.
pub const CONSOLE_CAPACITY: usize = 2048;

/// Severity of a console entry. The panel maps each to an icon and a
/// theme color; `Error` also auto-opens the Console tab (mirrors egui).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleSeverity {
    Info,
    Warning,
    Error,
}

/// One line in the Console tab. `context` identifies the run that
/// produced the message -- `plugin/<name>` for plugin ops,
/// `<data-file> / <template-file>` for template diagnostics -- mirroring
/// the egui `ConsoleEntry` shape.
#[derive(Clone, Debug)]
pub struct ConsoleEntry {
    pub timestamp: jiff::Timestamp,
    pub severity: ConsoleSeverity,
    pub context: String,
    pub message: String,
}

/// A console line before it is timestamped and pushed. Carried by the
/// template runner's `TemplateConsoleLog` event so the workspace can
/// forward each diagnostic through `Workspace::console_log` (which owns
/// the timestamping and the Error auto-open).
#[derive(Clone, Debug)]
pub struct ConsoleLogRecord {
    pub severity: ConsoleSeverity,
    pub context: String,
    pub message: String,
}

/// Read-view of the workspace console buffer, republished on each
/// `Workspace::console_log`. The [`ConsolePanel`] observes it and
/// re-renders; absent until the first entry is logged.
pub struct ConsoleLogGlobal(pub Vec<ConsoleEntry>);

impl Global for ConsoleLogGlobal {}
