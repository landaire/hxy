//! Template-runtime dispatch: pick a runtime by file extension,
//! resolve `#include`s sandboxed against the user templates dir,
//! kick off the parse+execute on a worker thread, and drain
//! completed runs into the file's [`crate::panels::template`] state.

#![cfg(not(target_arch = "wasm32"))]

use crate::app::ConsoleSeverity;
use crate::app::HxyApp;
use crate::files::FileId;

/// Dialog entrypoint: prompt the user for a template path, then run
/// it against the active file. Stops silently if either side is missing.
pub fn run_template_dialog(ctx: &egui::Context, app: &mut HxyApp) {
    let Some(id) = crate::app::active_file_id(app) else { return };
    let Some(path) = rfd::FileDialog::new().pick_file() else { return };
    run_template_from_path(ctx, app, id, path);
}

pub fn run_template_from_path(ctx: &egui::Context, app: &mut HxyApp, id: FileId, path: std::path::PathBuf) {
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("").to_owned();

    let data_name = app.files.get(&id).map(|f| f.display_name.clone()).unwrap_or_else(|| format!("file-{}", id.get()));
    let tpl_name =
        path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
    let console_ctx = format!("{data_name} / {tpl_name}");

    let Some(runtime) = app.template_runtime_for(&ext) else {
        let dir = crate::app::user_template_plugins_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "$DATA/hxy/template-plugins".to_owned());
        let msg = format!(
            "No template runtime is registered for .{ext} files.\nInstall a matching runtime component (.wasm) into:\n{dir}"
        );
        app.console_log(ConsoleSeverity::Error, &console_ctx, &msg);
        if let Some(file) = app.files.get_mut(&id) {
            file.template = Some(crate::panels::template::error_state(msg));
        }
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
                if let Some(file) = app.files.get_mut(&id) {
                    file.template = Some(crate::panels::template::error_state(msg));
                }
                return;
            }
        },
        None => match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("Failed to read template source {}: {e}", path.display());
                app.console_log(ConsoleSeverity::Error, &console_ctx, &msg);
                if let Some(file) = app.files.get_mut(&id) {
                    file.template = Some(crate::panels::template::error_state(msg));
                }
                return;
            }
        },
    };

    let Some(file) = app.files.get_mut(&id) else { return };
    let source = file.editor.source().clone();
    file.template = None;
    let (sender, inbox) = egui_inbox::UiInbox::channel_with_ctx(ctx);
    file.template_running =
        Some(crate::files::TemplateRun { inbox, template_name: tpl_name.clone(), started: jiff::Timestamp::now() });

    std::thread::spawn(move || {
        let outcome = match runtime.parse(source, &template_source) {
            Ok(parsed) => match parsed.execute(&[]) {
                Ok(tree) => crate::files::TemplateRunOutcome::Ok { parsed, tree },
                Err(e) => crate::files::TemplateRunOutcome::Err(format!("Execute failed: {e}")),
            },
            Err(e) => crate::files::TemplateRunOutcome::Err(format!("Parse failed: {e}")),
        };
        let _ = sender.send(outcome);
    });

    app.console_log(ConsoleSeverity::Info, &console_ctx, format!("running template `{tpl_name}`..."));
}

/// Pop completed template-run results off each file's inbox and
/// swap them into the file's `TemplateState`. Called once per frame;
/// `UiInbox::read` is non-blocking and yields only items that the
/// worker has already sent.
pub fn drain_template_runs(ctx: &egui::Context, app: &mut HxyApp) {
    let mut done: Vec<(FileId, crate::files::TemplateRunOutcome, String)> = Vec::new();
    for (id, file) in app.files.iter_mut() {
        let Some(run) = file.template_running.as_ref() else { continue };
        let outcomes: Vec<_> = run.inbox.read(ctx).collect();
        if outcomes.is_empty() {
            continue;
        }
        let tpl = run.template_name.clone();
        file.template_running = None;
        for outcome in outcomes {
            done.push((*id, outcome, tpl.clone()));
        }
    }

    for (id, outcome, tpl) in done {
        let data_name =
            app.files.get(&id).map(|f| f.display_name.clone()).unwrap_or_else(|| format!("file-{}", id.get()));
        let console_ctx = format!("{data_name} / {tpl}");
        match outcome {
            crate::files::TemplateRunOutcome::Ok { parsed, tree } => {
                let diagnostics = tree.diagnostics.clone();
                let state = crate::panels::template::new_state_from(parsed, tree);
                if let Some(file) = app.files.get_mut(&id) {
                    file.template = Some(state);
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
                if let Some(file) = app.files.get_mut(&id) {
                    file.template = Some(crate::panels::template::error_state(msg));
                }
            }
        }
    }
    let _ = ctx;
}

/// Drain queued "Run X.bt" toast acceptances and route each one
/// through [`run_template_from_path`].
pub fn drain_pending_template_runs(ctx: &egui::Context, app: &mut HxyApp) {
    let runs: Vec<crate::toasts::PendingTemplateRun> = app.pending_template_runs.drain(..).collect();
    for run in runs {
        run_template_from_path(ctx, app, run.file_id, run.template_path);
    }
}
