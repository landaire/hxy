//! Template-runtime dispatch: pick a runtime by file extension,
//! resolve `#include`s sandboxed against the user templates dir,
//! kick off the parse+execute on a worker thread, and drain
//! completed runs into the file's [`crate::panels::template`] state.

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;

use hxy_core::ByteLen;
use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;
use hxy_plugin_host::ParsedTemplate;
use hxy_plugin_host::template::Diagnostic;
use hxy_plugin_host::template::Node;
use hxy_plugin_host::template::ResultTree;
use hxy_vfs::HandlerError;

use crate::app::ConsoleSeverity;
use crate::app::HxyApp;
use crate::files::FileId;
use crate::files::TemplateInstance;
use crate::files::TemplateInstanceId;

/// Dialog entrypoint: prompt the user for a template path, then run
/// it against the active file. Stops silently if either side is missing.
pub fn run_template_dialog(ctx: &egui::Context, app: &mut HxyApp) {
    let Some(id) = crate::app::active_file_id(app) else { return };
    let Some(path) = rfd::FileDialog::new().pick_file() else { return };
    run_template_from_path(ctx, app, id, path, None);
}

/// Run `path` against `id`'s bytes. When `range` is `Some`, the runtime
/// only sees that slice (offset 0 there maps to `range.start()` of the
/// real file); when `None`, the template binds against the whole file.
pub fn run_template_from_path(
    ctx: &egui::Context,
    app: &mut HxyApp,
    id: FileId,
    path: std::path::PathBuf,
    range: Option<ByteRange>,
) {
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("").to_owned();

    let data_name = app.files.get(&id).map(|f| f.display_name.clone()).unwrap_or_else(|| format!("file-{}", id.get()));
    let tpl_name =
        path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
    let console_ctx = format!("{data_name} / {tpl_name}");

    let Some(file) = app.files.get_mut(&id) else { return };
    let source_len = file.editor.source().len();
    let bound_range = match range {
        Some(r) => r,
        None => match ByteRange::new(ByteOffset::new(0), ByteOffset::new(source_len.get())) {
            Ok(r) => r,
            Err(_) => return,
        },
    };
    if bound_range.end().get() > source_len.get() {
        let msg = format!(
            "Template range {}..{} exceeds source length {}.",
            bound_range.start().get(),
            bound_range.end().get(),
            source_len.get()
        );
        app.console_log(ConsoleSeverity::Error, &console_ctx, &msg);
        record_error_instance(app, id, &path, &tpl_name, bound_range, ctx, msg);
        return;
    }

    let Some(runtime) = app.template_runtime_for(&ext) else {
        let dir = crate::app::user_template_plugins_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "$DATA/hxy/template-plugins".to_owned());
        let msg = format!(
            "No template runtime is registered for .{ext} files.\nInstall a matching runtime component (.wasm) into:\n{dir}"
        );
        app.console_log(ConsoleSeverity::Error, &console_ctx, &msg);
        record_error_instance(app, id, &path, &tpl_name, bound_range, ctx, msg);
        return;
    };

    // Resolve `#include` textually before handing the source to the
    // runtime. Sandboxed to the user's templates directory so a
    // malicious template can't pull in arbitrary files via
    // `#include "../../..."`. Templates run directly from a path
    // outside the sandbox (e.g. in-tree fixtures) fall back to the
    // raw file with no expansion.
    let sandbox = crate::app::user_templates_dir();
    let template_source = match sandbox.as_deref().and_then(|base| {
        let canonical_base = base.canonicalize().ok()?;
        let canonical_path = path.canonicalize().ok()?;
        canonical_path.starts_with(&canonical_base).then_some(canonical_base)
    }) {
        Some(base) => match crate::templates::library::expand_includes(&path, &base) {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("Failed to read template source {}: {e}", path.display());
                app.console_log(ConsoleSeverity::Error, &console_ctx, &msg);
                record_error_instance(app, id, &path, &tpl_name, bound_range, ctx, msg);
                return;
            }
        },
        None => match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("Failed to read template source {}: {e}", path.display());
                app.console_log(ConsoleSeverity::Error, &console_ctx, &msg);
                record_error_instance(app, id, &path, &tpl_name, bound_range, ctx, msg);
                return;
            }
        },
    };

    let Some(file) = app.files.get_mut(&id) else { return };
    let source = file.editor.source().clone();
    file.last_template_path = Some(path.clone());
    let instance_id = file.fresh_template_instance_id();
    let full_file = bound_range.start().get() == 0 && bound_range.len().get() == source_len.get();
    let bound_source: Arc<dyn HexSource> = if full_file {
        source
    } else {
        Arc::new(SubrangeSource::new(source, bound_range))
    };
    let (sender, inbox) = egui_inbox::UiInbox::channel_with_ctx(ctx);
    file.templates_running.push(crate::files::TemplateRunInstance {
        id: instance_id,
        source_path: path.clone(),
        display_name: tpl_name.clone(),
        range: bound_range,
        run: crate::files::TemplateRun {
            inbox,
            template_name: tpl_name.clone(),
            started: jiff::Timestamp::now(),
        },
    });
    file.active_template = Some(instance_id);

    let base = bound_range.start().get();
    std::thread::spawn(move || {
        let outcome = match runtime.parse(bound_source, &template_source) {
            Ok(parsed) => {
                let adjusted: Arc<dyn ParsedTemplate> = Arc::new(OffsetAdjustedTemplate { inner: parsed, base });
                match adjusted.execute(&[]) {
                    Ok(tree) => crate::files::TemplateRunOutcome::Ok { parsed: adjusted, tree },
                    Err(e) => crate::files::TemplateRunOutcome::Err(format!("Execute failed: {e}")),
                }
            }
            Err(e) => crate::files::TemplateRunOutcome::Err(format!("Parse failed: {e}")),
        };
        let _ = sender.send(outcome);
    });

    app.console_log(ConsoleSeverity::Info, &console_ctx, format!("running template `{tpl_name}`..."));
}

/// Pop completed template-run results off each file's running list and
/// swap them into the file's `templates`. Called once per frame;
/// `UiInbox::read` is non-blocking and yields only items that the
/// worker has already sent.
pub fn drain_template_runs(ctx: &egui::Context, app: &mut HxyApp) {
    struct Done {
        file_id: FileId,
        instance_id: TemplateInstanceId,
        source_path: std::path::PathBuf,
        display_name: String,
        range: ByteRange,
        outcome: crate::files::TemplateRunOutcome,
    }
    let mut done: Vec<Done> = Vec::new();

    for (id, file) in app.files.iter_mut() {
        let mut still_running: Vec<crate::files::TemplateRunInstance> =
            Vec::with_capacity(file.templates_running.len());
        for running in std::mem::take(&mut file.templates_running) {
            let outcomes: Vec<_> = running.run.inbox.read(ctx).collect();
            if outcomes.is_empty() {
                still_running.push(running);
                continue;
            }
            for outcome in outcomes {
                done.push(Done {
                    file_id: *id,
                    instance_id: running.id,
                    source_path: running.source_path.clone(),
                    display_name: running.display_name.clone(),
                    range: running.range,
                    outcome,
                });
            }
        }
        file.templates_running = still_running;
    }

    for entry in done {
        let data_name = app
            .files
            .get(&entry.file_id)
            .map(|f| f.display_name.clone())
            .unwrap_or_else(|| format!("file-{}", entry.file_id.get()));
        let console_ctx = format!("{data_name} / {}", entry.display_name);
        match entry.outcome {
            crate::files::TemplateRunOutcome::Ok { parsed, tree } => {
                let diagnostics = tree.diagnostics.clone();
                let state = crate::panels::template::new_state_from(parsed, tree);
                if let Some(file) = app.files.get_mut(&entry.file_id) {
                    upsert_instance(file, entry.instance_id, &entry.source_path, &entry.display_name, entry.range, state);
                }
                for d in &diagnostics {
                    let severity = match d.severity {
                        hxy_plugin_host::template::Severity::Error => ConsoleSeverity::Error,
                        hxy_plugin_host::template::Severity::Warning => ConsoleSeverity::Warning,
                        hxy_plugin_host::template::Severity::Info => ConsoleSeverity::Info,
                    };
                    let loc = match d.file_offset {
                        Some(off) => format!(" @ {off:#x}"),
                        None => String::new(),
                    };
                    app.console_log(severity, &console_ctx, format!("{}{}", d.message, loc));
                }
                if diagnostics.is_empty() {
                    app.console_log(ConsoleSeverity::Info, &console_ctx, "template executed successfully");
                }
            }
            crate::files::TemplateRunOutcome::Err(msg) => {
                app.console_log(ConsoleSeverity::Error, &console_ctx, &msg);
                if let Some(file) = app.files.get_mut(&entry.file_id) {
                    let state = crate::panels::template::error_state(msg);
                    upsert_instance(
                        file,
                        entry.instance_id,
                        &entry.source_path,
                        &entry.display_name,
                        entry.range,
                        state,
                    );
                }
            }
        }
    }
    let _ = ctx;
}

/// Drain queued "Run X.bt" toast acceptances and route each one
/// through [`run_template_from_path`]. Toast suggestions always run
/// against the whole file, so no range is supplied.
pub fn drain_pending_template_runs(ctx: &egui::Context, app: &mut HxyApp) {
    let runs: Vec<crate::toasts::PendingTemplateRun> = app.pending_template_runs.drain(..).collect();
    for run in runs {
        run_template_from_path(ctx, app, run.file_id, run.template_path, None);
    }
}

/// Insert or replace a template instance under a known id. Used by the
/// drain step (success / error) so a re-run can re-bind into the same
/// tab without disturbing surrounding tabs.
fn upsert_instance(
    file: &mut crate::files::OpenFile,
    instance_id: TemplateInstanceId,
    source_path: &std::path::Path,
    display_name: &str,
    range: ByteRange,
    state: crate::files::TemplateState,
) {
    let new_instance = TemplateInstance {
        id: instance_id,
        source_path: source_path.to_path_buf(),
        display_name: display_name.to_owned(),
        range,
        state,
    };
    if let Some(slot) = file.templates.iter_mut().find(|t| t.id == instance_id) {
        *slot = new_instance;
    } else {
        file.templates.push(new_instance);
    }
    if file.active_template.is_none() {
        file.active_template = Some(instance_id);
    }
}

/// Synchronously install a diagnostics-only template instance under a
/// fresh id. Used when we know up-front that the run can't proceed
/// (no runtime, include resolution failed, range out of bounds).
fn record_error_instance(
    app: &mut HxyApp,
    id: FileId,
    path: &std::path::Path,
    display_name: &str,
    range: ByteRange,
    _ctx: &egui::Context,
    message: String,
) {
    let Some(file) = app.files.get_mut(&id) else { return };
    let instance_id = file.fresh_template_instance_id();
    let state = crate::panels::template::error_state(message);
    upsert_instance(file, instance_id, path, display_name, range, state);
    file.active_template = Some(instance_id);
}

/// View a sub-range of an inner [`HexSource`] as if it were the whole
/// thing. Reads at offset `0` of the wrapper map to `base` of the
/// inner; `len()` is the sub-range's length. Used so a template
/// runtime sees the slice as offsets `[0, len)` and emits node spans
/// rooted at `0`. The runner re-anchors those spans to the real file
/// via [`OffsetAdjustedTemplate`].
struct SubrangeSource {
    inner: Arc<dyn HexSource>,
    base: ByteOffset,
    len: ByteLen,
}

impl SubrangeSource {
    fn new(inner: Arc<dyn HexSource>, range: ByteRange) -> Self {
        Self { inner, base: range.start(), len: range.len() }
    }
}

impl HexSource for SubrangeSource {
    fn len(&self) -> ByteLen {
        self.len
    }

    fn read(&self, range: ByteRange) -> Result<Vec<u8>, hxy_core::Error> {
        if range.end().get() > self.len.get() {
            return Err(hxy_core::Error::OutOfBounds {
                range,
                len: ByteOffset::new(self.len.get()),
            });
        }
        let inner_start = ByteOffset::new(self.base.get() + range.start().get());
        let inner_end = ByteOffset::new(self.base.get() + range.end().get());
        let inner_range = ByteRange::new(inner_start, inner_end)?;
        self.inner.read(inner_range)
    }
}

/// Wrap a [`ParsedTemplate`] so every emitted node's `span.offset` and
/// every diagnostic's `file_offset` is shifted by `base`. Lets the
/// rest of the app treat template node offsets as file-absolute
/// regardless of whether the template was bound to a slice.
struct OffsetAdjustedTemplate {
    inner: Arc<dyn ParsedTemplate>,
    base: u64,
}

impl ParsedTemplate for OffsetAdjustedTemplate {
    fn execute(&self, args: &[hxy_plugin_host::template::Arg]) -> Result<ResultTree, HandlerError> {
        let mut tree = self.inner.execute(args)?;
        adjust_tree(&mut tree, self.base);
        Ok(tree)
    }

    fn expand_array(&self, array_id: u64, start: u64, end: u64) -> Result<Vec<Node>, HandlerError> {
        let mut nodes = self.inner.expand_array(array_id, start, end)?;
        for node in &mut nodes {
            adjust_node_span(node, self.base);
        }
        Ok(nodes)
    }
}

fn adjust_tree(tree: &mut ResultTree, base: u64) {
    if base == 0 {
        return;
    }
    for node in &mut tree.nodes {
        adjust_node_span(node, base);
    }
    for diag in &mut tree.diagnostics {
        adjust_diagnostic(diag, base);
    }
}

fn adjust_node_span(node: &mut Node, base: u64) {
    node.span.offset = node.span.offset.saturating_add(base);
}

fn adjust_diagnostic(diag: &mut Diagnostic, base: u64) {
    if let Some(off) = diag.file_offset {
        diag.file_offset = Some(off.saturating_add(base));
    }
}
