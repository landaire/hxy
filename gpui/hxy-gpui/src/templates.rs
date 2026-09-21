//! Template runtime registry and background template runner.
//!
//! Ports the egui runner's orchestration
//! (`crates/hxy/src/templates/runner.rs`) onto gpui: preflight
//! (runtime dispatch, range check, sandboxed include expansion,
//! source fingerprinting) runs synchronously, then parse+execute
//! runs on the background executor and the outcome lands back on
//! the owning [`FilePanel`] by entity update -- no per-frame drain.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::App;
use gpui::AppContext;
use gpui::Context;
use gpui::Global;
use gpui::Hsla;
use gpui::Task;
use gpui::Window;
use gpui::component::WindowExt;
use gpui::component::notification::Notification;
use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;
use hxy_plugin_host::ParsedTemplate;
use hxy_plugin_host::TemplateRuntime;
use hxy_plugin_host::template::ResultTree;
use hxy_plugin_host::template::Severity;
use hxy_templates::color::Rgba;
use hxy_templates::library::expand_includes;
use hxy_templates::run::OffsetAdjustedTemplate;
use hxy_templates::run::SubrangeSource;
use hxy_templates::run::fingerprint_template_source;
use hxy_templates::state::TemplateInstance;
use hxy_templates::state::TemplateInstanceId;
use hxy_templates::state::error_state;
use hxy_templates::state::new_state_from;
use hxy_templates::user_template_plugins_dir;
use hxy_templates::user_templates_dir;

use crate::console::ConsoleLogRecord;
use crate::console::ConsoleSeverity;
use crate::panels::FilePanel;
use crate::panels::TemplateConsoleLog;

/// Every loaded template runtime. User-installed WASM components sit
/// ahead of the builtins so a user component can override a builtin
/// for the same extension (first match wins).
pub struct TemplateRuntimes(pub Vec<Arc<dyn TemplateRuntime>>);

impl Global for TemplateRuntimes {}

/// The auto-detected template library (user templates directory plus
/// the fetched ImHex-Patterns corpus), loaded at startup and
/// refreshed after install / uninstall / pattern fetch. The palette
/// ranks its entries against the active file.
pub struct TemplateLibraryGlobal(pub hxy_templates::library::TemplateLibrary);

impl Global for TemplateLibraryGlobal {}

/// Scan the default template directories into a fresh library global.
pub fn load_library() -> TemplateLibraryGlobal {
    TemplateLibraryGlobal(hxy_templates::library::TemplateLibrary::load_default())
}

/// Re-scan the template directories after something changed on disk
/// (install, uninstall, pattern fetch).
pub fn refresh_library(cx: &mut App) {
    cx.set_global(load_library());
}

/// Rebuild the template-runtime registry after a runtime component was
/// installed or deleted (the plugins panel's template-runtimes
/// section). Failures are logged inside [`load_runtimes`], never fatal.
pub fn refresh_runtimes(cx: &mut App) {
    cx.set_global(load_runtimes());
}

/// Direction of a template field jump (palette "Jump to next/previous
/// template field").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldJump {
    Next,
    Prev,
}

impl TemplateRuntimes {
    /// First runtime claiming `ext` (case-insensitive). Ports the
    /// egui app's `template_runtime_for`.
    pub fn runtime_for(&self, ext: &str) -> Option<Arc<dyn TemplateRuntime>> {
        self.0.iter().find(|r| r.extensions().iter().any(|e| e.eq_ignore_ascii_case(ext))).cloned()
    }
}

/// Build the startup registry: builtin runtimes plus user-installed
/// WASM components from the shared plugin directory. Ports the egui
/// app's `load_user_template_plugins`; load failures are logged,
/// never fatal.
pub fn load_runtimes() -> TemplateRuntimes {
    let mut out: Vec<Arc<dyn TemplateRuntime>> = Vec::new();
    for rt in hxy_templates::builtin::builtins() {
        tracing::info!(name = rt.name(), exts = ?rt.extensions(), builtin = true, "loaded template runtime");
        out.push(rt);
    }
    // A template runtime present in more than one dir (data dir plus a
    // dev bundle) registers once; the first dir wins. Seeded empty, not
    // with the builtins above, so a plugin runtime still overrides a
    // same-named builtin (inserted at the front).
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for dir in template_plugin_dirs() {
        match hxy_plugin_host::load_template_plugins_from_dir(&dir) {
            Ok(runtimes) => {
                for r in runtimes {
                    if !seen.insert(r.name().to_owned()) {
                        tracing::debug!(name = r.name(), "skip duplicate template runtime from a later dir");
                        continue;
                    }
                    tracing::info!(name = r.name(), exts = ?r.extensions(), builtin = false, "loaded template runtime");
                    out.insert(0, Arc::new(r));
                }
            }
            Err(e) => tracing::warn!(error = %e, dir = %dir.display(), "load template runtimes"),
        }
    }
    TemplateRuntimes(out)
}

/// Every directory scanned for template-runtime components: the shared
/// data dir, plus a `template-plugins/` dir next to the executable in
/// debug builds (so a buck2 dev bundle ships them beside the binary).
fn template_plugin_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    // Debug: the bundle beside the executable wins over the data dir, so a
    // rebuilt runtime is what loads.
    #[cfg(debug_assertions)]
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.join("template-plugins"))) {
        dirs.push(dir);
    }
    if let Some(dir) = user_template_plugins_dir() {
        dirs.push(dir);
    }
    dirs
}

/// Convert the shared template color newtype into a gpui [`Hsla`].
/// [`Rgba`] stores premultiplied sRGBA bytes (Color32-compatible);
/// gpui wants straight alpha, so translucent colors un-multiply on
/// the way through. Used for swatches now and hex-view tints in the
/// styler composition (M4a Task 5).
pub fn rgba_to_hsla(c: Rgba) -> Hsla {
    let (r, g, b) = match c.a {
        // Fully transparent has no color to recover; fully opaque
        // is already straight.
        0 => (0, 0, 0),
        255 => (c.r, c.g, c.b),
        a => {
            let un = |v: u8| ((f32::from(v) * 255.0 / f32::from(a)).round().min(255.0)) as u8;
            (un(c.r), un(c.g), un(c.b))
        }
    };
    Hsla::from(gpui::Rgba {
        r: f32::from(r) / 255.0,
        g: f32::from(g) / 255.0,
        b: f32::from(b) / 255.0,
        a: f32::from(c.a) / 255.0,
    })
}

/// Restart-time context for an auto-rerun. The restore path passes
/// the previous session's fingerprint and color overrides; the
/// runner applies the overrides only when the freshly computed
/// fingerprint still matches (template source unchanged on disk).
#[derive(Clone, Default)]
pub struct RestoreContext {
    pub expected_fingerprint: Option<[u8; 32]>,
    pub overrides: HashMap<u32, Rgba>,
}

/// One in-flight template run on a [`FilePanel`]. Dropping the
/// handle cancels the background task.
pub struct TemplateRunHandle {
    pub id: TemplateInstanceId,
    pub display_name: String,
    pub started: jiff::Timestamp,
    _task: Task<()>,
}

/// Why a run could not start. Produced by the synchronous preflight;
/// [`run_template`] converts it to a localized message for the error
/// instance and toast at that boundary.
#[derive(Debug, thiserror::Error)]
pub enum RunTemplateError {
    #[error("no template runtime registered for extension {ext:?}")]
    NoRuntime { ext: String },
    #[error("template range {start}..{end} exceeds source length {len}")]
    RangeOutOfBounds { start: u64, end: u64, len: u64 },
    #[error("failed to read template source {}: {source}", path.display())]
    ReadSource {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Run the template at `path` against `panel`'s bytes. When `range`
/// is `Some`, the runtime only sees that slice (offset 0 there maps
/// to `range.start()` of the real file); when `None`, the template
/// binds against the whole file. Preflight failures land as a
/// diagnostics-only instance plus an error toast. Returns the id the
/// run (or error instance) was allocated under, so a restore can map
/// its persisted active index back onto live instances. `None` only
/// on the defensive whole-file range failure below, which cannot
/// fire (`ByteRange::new` accepts any `start <= end`, and `0 <= len`
/// always holds -- an empty source yields the valid range `0..0`).
pub fn run_template(
    panel: &mut FilePanel,
    path: PathBuf,
    range: Option<ByteRange>,
    restore: RestoreContext,
    window: &mut Window,
    cx: &mut Context<FilePanel>,
) -> Option<TemplateInstanceId> {
    let tpl_name = display_name_for(&path);
    let source = panel.pane().read(cx).editor().source().clone();
    let source_len = source.len().get();
    let bound_range = match range {
        Some(r) => r,
        // 0..len over an existing source is always a valid range.
        None => ByteRange::new(ByteOffset::new(0), ByteOffset::new(source_len)).ok()?,
    };
    match prepare(&path, bound_range, source_len, cx) {
        Ok(prepared) => {
            Some(spawn_run(panel, path, tpl_name, prepared, bound_range, source, source_len, restore, window, cx))
        }
        Err(err) => {
            let message = error_message(&err);
            let id = record_error_instance(panel, &path, &tpl_name, bound_range, message.clone(), cx);
            window.push_notification(Notification::error(message), cx);
            Some(id)
        }
    }
}

/// Runtime plus expanded template source, resolved before anything
/// is queued so preflight failures never allocate an instance id.
struct PreparedRun {
    runtime: Arc<dyn TemplateRuntime>,
    template_source: String,
}

fn prepare(path: &Path, bound_range: ByteRange, source_len: u64, cx: &App) -> Result<PreparedRun, RunTemplateError> {
    if bound_range.end().get() > source_len {
        return Err(RunTemplateError::RangeOutOfBounds {
            start: bound_range.start().get(),
            end: bound_range.end().get(),
            len: source_len,
        });
    }
    // A missing extension is just an unclaimed one: no runtime
    // matches the empty string, yielding the no-runtime error.
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    let Some(runtime) = cx.global::<TemplateRuntimes>().runtime_for(ext) else {
        return Err(RunTemplateError::NoRuntime { ext: ext.to_owned() });
    };
    // Resolve `#include` textually before handing the source to the
    // runtime, sandboxed to the user's templates directory so a
    // malicious template can't pull in arbitrary files. Templates
    // run from a path outside the sandbox (e.g. fixtures) fall back
    // to the raw file with no expansion.
    let read = match user_templates_dir().as_deref().and_then(|base| {
        let canonical_base = base.canonicalize().ok()?;
        let canonical_path = path.canonicalize().ok()?;
        canonical_path.starts_with(&canonical_base).then_some(canonical_base)
    }) {
        Some(base) => expand_includes(path, &base),
        None => std::fs::read_to_string(path),
    };
    let template_source = read.map_err(|source| RunTemplateError::ReadSource { path: path.to_path_buf(), source })?;
    Ok(PreparedRun { runtime, template_source })
}

#[allow(clippy::too_many_arguments)]
fn spawn_run(
    panel: &mut FilePanel,
    path: PathBuf,
    tpl_name: String,
    prepared: PreparedRun,
    bound_range: ByteRange,
    source: Arc<dyn HexSource>,
    source_len: u64,
    restore: RestoreContext,
    window: &mut Window,
    cx: &mut Context<FilePanel>,
) -> TemplateInstanceId {
    let PreparedRun { runtime, template_source } = prepared;
    // Hash the expanded source the worker is about to consume: that
    // is the content the resulting node indices reflect, so it's the
    // right key for "are last session's overrides still valid?".
    let source_fingerprint = fingerprint_template_source(&template_source);
    let overrides = match restore.expected_fingerprint {
        Some(expected) if Some(expected) == source_fingerprint => restore.overrides,
        Some(_) => {
            tracing::info!(
                template = %tpl_name,
                "template source changed since last session; dropping persisted color overrides"
            );
            HashMap::new()
        }
        None => restore.overrides,
    };
    panel.last_template_path = Some(path.clone());
    let instance_id = panel.fresh_template_instance_id();
    let full_file = bound_range.start().get() == 0 && bound_range.len().get() == source_len;
    let bound_source: Arc<dyn HexSource> =
        if full_file { source } else { Arc::new(SubrangeSource::new(source, bound_range)) };
    let base = bound_range.start().get();
    let display_name = tpl_name.clone();
    let task = cx.spawn_in(window, async move |this, cx| {
        let outcome = cx.background_spawn(async move { execute(runtime, bound_source, &template_source, base) }).await;
        let _ = this.update_in(cx, |this, window, cx| {
            this.templates_running.retain(|h| h.id != instance_id);
            let context = console_context(this, &display_name);
            let mut records: Vec<ConsoleLogRecord> = Vec::new();
            let (fingerprint, state, error_toasts) = match outcome {
                Ok((parsed, tree)) => {
                    // Every diagnostic reaches the console; only errors
                    // also toast (mirrors egui's console + toast split).
                    for d in &tree.diagnostics {
                        records.push(ConsoleLogRecord {
                            severity: console_severity(d.severity),
                            context: context.clone(),
                            message: d.message.clone(),
                        });
                    }
                    let toasts: Vec<String> = tree
                        .diagnostics
                        .iter()
                        .filter(|d| matches!(d.severity, Severity::Error))
                        .map(|d| {
                            hxy_i18n::t_args(
                                "gpui-template-diagnostic-error",
                                &[("template", &display_name), ("message", &d.message)],
                            )
                        })
                        .collect();
                    (source_fingerprint, new_state_from(parsed, tree, overrides), toasts)
                }
                Err(message) => {
                    records.push(ConsoleLogRecord {
                        severity: ConsoleSeverity::Error,
                        context: context.clone(),
                        message: message.clone(),
                    });
                    (None, error_state(message.clone()), vec![message])
                }
            };
            this.upsert_template_instance(TemplateInstance {
                id: instance_id,
                source_path: path,
                display_name,
                range: bound_range,
                source_fingerprint: fingerprint,
                state,
            });
            for text in error_toasts {
                window.push_notification(Notification::error(text), cx);
            }
            if !records.is_empty() {
                cx.emit(TemplateConsoleLog(records));
            }
            this.sync_template_rows(cx);
            // A hover band from the previously active instance must
            // not linger once the finished run becomes active.
            this.sync_pane_overlays(cx);
            // An edit landed while this run was in flight: replay one
            // re-run against the now-current bytes.
            if this.templates_running.is_empty() && std::mem::take(&mut this.template_rerun_pending) {
                this.rerun_templates(window, cx);
            }
            cx.notify();
        });
    });
    panel.templates_running.push(TemplateRunHandle {
        id: instance_id,
        display_name: tpl_name,
        started: jiff::Timestamp::now(),
        _task: task,
    });
    panel.active_template = Some(instance_id);
    panel.template_panel_visible = true;
    panel.sync_template_rows(cx);
    // The running instance has no state yet, so the previous active
    // instance's tints/palette must not linger while it computes.
    panel.sync_pane_overlays(cx);
    cx.notify();
    instance_id
}

/// Background half of a run: parse, re-anchor spans to the bound
/// range's base, execute. Error strings are pre-localized here so
/// the completion handler can toast them and store them in the
/// error instance verbatim (same shape the egui runner used).
fn execute(
    runtime: Arc<dyn TemplateRuntime>,
    source: Arc<dyn HexSource>,
    template_source: &str,
    base: u64,
) -> Result<(Arc<dyn ParsedTemplate>, ResultTree), String> {
    let parsed = runtime
        .parse(source, template_source)
        .map_err(|e| hxy_i18n::t_args("gpui-template-parse-failed", &[("error", &e.to_string())]))?;
    let adjusted: Arc<dyn ParsedTemplate> = Arc::new(OffsetAdjustedTemplate { inner: parsed, base });
    let tree = adjusted
        .execute(&[])
        .map_err(|e| hxy_i18n::t_args("gpui-template-execute-failed", &[("error", &e.to_string())]))?;
    Ok((adjusted, tree))
}

/// Install a diagnostics-only instance under a fresh id when the run
/// can't proceed (no runtime, unreadable source, range out of
/// bounds). Mirrors the egui runner's `record_error_instance`.
fn record_error_instance(
    panel: &mut FilePanel,
    path: &Path,
    display_name: &str,
    range: ByteRange,
    message: String,
    cx: &mut Context<FilePanel>,
) -> TemplateInstanceId {
    let instance_id = panel.fresh_template_instance_id();
    cx.emit(TemplateConsoleLog(vec![ConsoleLogRecord {
        severity: ConsoleSeverity::Error,
        context: console_context(panel, display_name),
        message: message.clone(),
    }]));
    panel.upsert_template_instance(TemplateInstance {
        id: instance_id,
        source_path: path.to_path_buf(),
        display_name: display_name.to_owned(),
        range,
        source_fingerprint: None,
        state: error_state(message),
    });
    panel.active_template = Some(instance_id);
    panel.template_panel_visible = true;
    panel.sync_template_rows(cx);
    // An error instance has no tree; drop the previous instance's
    // tints/palette so the hex view matches the now-active state.
    panel.sync_pane_overlays(cx);
    cx.notify();
    instance_id
}

fn error_message(err: &RunTemplateError) -> String {
    match err {
        RunTemplateError::NoRuntime { ext } => {
            // Display-only fallback for the rare case the platform
            // data dir can't be resolved; mirrors the egui runner.
            let dir = user_template_plugins_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "$DATA/hxy/template-plugins".to_owned());
            hxy_i18n::t_args("gpui-template-no-runtime", &[("ext", ext), ("dir", &dir)])
        }
        RunTemplateError::RangeOutOfBounds { start, end, len } => hxy_i18n::t_args(
            "gpui-template-range-out-of-bounds",
            &[("start", &start.to_string()), ("end", &end.to_string()), ("len", &len.to_string())],
        ),
        RunTemplateError::ReadSource { path, source } => hxy_i18n::t_args(
            "gpui-template-read-failed",
            &[("path", &path.display().to_string()), ("error", &source.to_string())],
        ),
    }
}

/// The template's tab-strip name: its file leaf name, falling back
/// to the full path when it has none.
fn display_name_for(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

/// Console `context` for a template run: `<data-file> / <template>`,
/// falling back to just the template name for an untitled buffer.
/// Mirrors the egui `ConsoleEntry.context` convention.
fn console_context(panel: &FilePanel, template: &str) -> String {
    match panel.path().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()) {
        Some(data) => format!("{data} / {template}"),
        None => template.to_owned(),
    }
}

/// Map a template diagnostic severity onto the console severity.
fn console_severity(severity: Severity) -> ConsoleSeverity {
    match severity {
        Severity::Error => ConsoleSeverity::Error,
        Severity::Warning => ConsoleSeverity::Warning,
        Severity::Info => ConsoleSeverity::Info,
    }
}

#[cfg(test)]
mod tests {
    use gpui::Entity;
    use gpui::TestAppContext;
    use hxy_core::MemorySource;
    use hxy_vfs::HandlerError;

    use super::*;

    struct FakeRuntime {
        name: &'static str,
        exts: Vec<String>,
    }

    impl TemplateRuntime for FakeRuntime {
        fn name(&self) -> &str {
            self.name
        }

        fn extensions(&self) -> &[String] {
            &self.exts
        }

        fn parse(
            &self,
            _source: Arc<dyn HexSource>,
            _template_source: &str,
        ) -> Result<Arc<dyn ParsedTemplate>, HandlerError> {
            Err(HandlerError::Unsupported("fake runtime never parses".into()))
        }
    }

    #[test]
    fn runtime_for_is_case_insensitive_and_first_wins() {
        let registry = TemplateRuntimes(vec![
            Arc::new(FakeRuntime { name: "first", exts: vec!["bt".to_owned()] }),
            Arc::new(FakeRuntime { name: "second", exts: vec!["bt".to_owned(), "hexpat".to_owned()] }),
        ]);
        assert_eq!(registry.runtime_for("bt").expect("claimed").name(), "first");
        assert_eq!(registry.runtime_for("BT").expect("case-insensitive").name(), "first");
        assert_eq!(registry.runtime_for("HexPat").expect("claimed").name(), "second");
        assert!(registry.runtime_for("xyz").is_none());
    }

    fn setup(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui::component::init(cx);
            cx.set_global(TemplateRuntimes(hxy_templates::builtin::builtins()));
        });
    }

    /// A `FilePanel` inside a real `gpui::component::Root` window --
    /// the run flow's toasts need the Root notification layer.
    fn build(cx: &mut TestAppContext, bytes: Vec<u8>) -> (Entity<FilePanel>, &mut gpui::VisualTestContext) {
        let window = cx.add_window(|window, cx| {
            let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
            let panel = cx.new(|cx| FilePanel::new(source, None, window, cx));
            gpui::component::Root::new(panel, window, cx)
        });
        let root = window.root(cx).unwrap();
        let panel = root.read_with(cx, |root, _| root.view().clone().downcast::<FilePanel>().unwrap());
        let vcx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        vcx.run_until_parked();
        (panel, vcx)
    }

    fn write_template(dir: &tempfile::TempDir, name: &str, source: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, source).unwrap();
        path
    }

    fn run(panel: &Entity<FilePanel>, cx: &mut gpui::VisualTestContext, path: PathBuf, range: Option<ByteRange>) {
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                run_template(panel, path, range, RestoreContext::default(), window, cx);
            });
        });
        cx.run_until_parked();
    }

    /// Whole-file run of an inline `.bt` template: the completed
    /// instance lands with absolute spans, becomes active, and the
    /// running list drains.
    #[gpui::test]
    fn run_template_lands_completed_instance(cx: &mut TestAppContext) {
        setup(cx);
        let mut bytes = 0xAABBCCDDu32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&0x11223344u32.to_le_bytes());
        let (panel, cx) = build(cx, bytes);

        let dir = tempfile::tempdir().unwrap();
        let path = write_template(&dir, "pair.bt", "LittleEndian();\nuint32 a;\nuint32 b;\n");
        run(&panel, cx, path.clone(), None);

        panel.read_with(cx, |panel, _| {
            assert!(panel.templates_running.is_empty(), "running list drained");
            assert_eq!(panel.templates.len(), 1);
            let instance = &panel.templates[0];
            assert_eq!(instance.source_path, path);
            assert_eq!(instance.display_name, "pair.bt");
            assert!(instance.source_fingerprint.is_some());
            assert!(instance.state.parsed.is_some(), "successful run keeps the parsed template");
            assert_eq!(panel.active_template, Some(instance.id));
            assert!(panel.template_panel_visible);
            let offsets: Vec<u64> = instance.state.tree.nodes.iter().map(|n| n.span.offset).collect();
            assert_eq!(offsets, vec![0, 4], "whole-file spans start at 0");
            assert_eq!(panel.last_template_path.as_deref(), Some(path.as_path()));
        });
    }

    /// Unknown extension: no runtime claims it, so a diagnostics-only
    /// error instance lands synchronously.
    #[gpui::test]
    fn unknown_extension_yields_error_instance(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx, vec![0u8; 8]);

        let dir = tempfile::tempdir().unwrap();
        let path = write_template(&dir, "mystery.xyz", "uint32 a;\n");
        run(&panel, cx, path, None);

        panel.read_with(cx, |panel, _| {
            assert!(panel.templates_running.is_empty());
            assert_eq!(panel.templates.len(), 1);
            let instance = &panel.templates[0];
            assert!(instance.state.parsed.is_none(), "error instance has no parsed template");
            assert_eq!(instance.source_fingerprint, None);
            assert_eq!(instance.state.tree.diagnostics.len(), 1);
            assert!(matches!(instance.state.tree.diagnostics[0].severity, Severity::Error));
            assert_eq!(panel.active_template, Some(instance.id));
        });
    }

    /// Slice run: the runtime sees the sub-range as offsets `[0, len)`
    /// and the landed spans are re-anchored to the slice's file
    /// position.
    #[gpui::test]
    fn slice_run_offsets_spans_by_range_start(cx: &mut TestAppContext) {
        setup(cx);
        let mut bytes = vec![0xFF; 4];
        bytes.extend_from_slice(&0xAABBCCDDu32.to_le_bytes());
        bytes.extend_from_slice(&[0xFF; 4]);
        let (panel, cx) = build(cx, bytes);

        let dir = tempfile::tempdir().unwrap();
        let path = write_template(&dir, "one.bt", "LittleEndian();\nuint32 v;\n");
        let range = ByteRange::new(ByteOffset::new(4), ByteOffset::new(8)).unwrap();
        run(&panel, cx, path, Some(range));

        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.templates.len(), 1);
            let instance = &panel.templates[0];
            assert_eq!(instance.range, range);
            let offsets: Vec<u64> = instance.state.tree.nodes.iter().map(|n| n.span.offset).collect();
            assert_eq!(offsets, vec![4], "span re-anchored to the slice start");
            // The 0xFF sentinels surround the slice: decoding the
            // slice's value proves the runtime read the sub-range,
            // not the whole file shifted by base.
            let value = instance.state.tree.nodes[0].value.clone();
            assert!(
                matches!(value, Some(hxy_plugin_host::template::Value::U32Val(0xAABBCCDD))),
                "runtime decoded the slice bytes, got {value:?}"
            );
        });
    }

    /// A matching restore fingerprint carries the persisted color
    /// overrides into the new state; a mismatch (template edited
    /// since last session) drops them.
    #[gpui::test]
    fn restore_overrides_survive_only_on_fingerprint_match(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let source = "LittleEndian();\nuint32 a;\n";
        let path = write_template(&dir, "one.bt", source);
        let overrides: HashMap<u32, Rgba> = HashMap::from([(0, Rgba::rgb(1, 2, 3))]);

        let (panel, cx) = build(cx, vec![0u8; 4]);
        // Outside the sandbox the runner reads the raw file, so the
        // run's fingerprint is the hash of `source` verbatim.
        let matching =
            RestoreContext { expected_fingerprint: fingerprint_template_source(source), overrides: overrides.clone() };
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| run_template(panel, path.clone(), None, matching, window, cx));
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.templates[0].state.node_color_overrides, overrides,
                "matching fingerprint keeps overrides"
            );
        });

        let (panel, cx) = build(cx, vec![0u8; 4]);
        let mismatched = RestoreContext { expected_fingerprint: Some([0u8; 32]), overrides };
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| run_template(panel, path, None, mismatched, window, cx));
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.templates[0].state.node_color_overrides.is_empty(),
                "stale fingerprint drops persisted overrides"
            );
        });
    }

    /// A user range beyond the source's length is rejected up front
    /// with an error instance instead of a doomed background run.
    #[gpui::test]
    fn out_of_bounds_range_yields_error_instance(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx, vec![0u8; 4]);

        let dir = tempfile::tempdir().unwrap();
        let path = write_template(&dir, "one.bt", "uint32 v;\n");
        let range = ByteRange::new(ByteOffset::new(0), ByteOffset::new(64)).unwrap();
        run(&panel, cx, path, Some(range));

        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.templates.len(), 1);
            assert!(panel.templates[0].state.parsed.is_none());
            assert_eq!(panel.templates[0].state.tree.diagnostics.len(), 1);
        });
    }

    /// Each template diagnostic severity maps onto the matching console
    /// severity that `on_template_console_log` forwards.
    #[test]
    fn console_severity_maps_every_diagnostic_severity() {
        assert_eq!(console_severity(Severity::Error), ConsoleSeverity::Error);
        assert_eq!(console_severity(Severity::Warning), ConsoleSeverity::Warning);
        assert_eq!(console_severity(Severity::Info), ConsoleSeverity::Info);
    }

    /// A `MemorySource`-backed panel has no on-disk path, so the console
    /// context is just the template name (no `<data> / <template>` prefix).
    #[gpui::test]
    fn console_context_without_a_data_path_is_the_template_name(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx, vec![0u8; 4]);
        let ctx = panel.read_with(cx, |panel, _| console_context(panel, "pair.bt"));
        assert_eq!(ctx, "pair.bt");
    }
}
