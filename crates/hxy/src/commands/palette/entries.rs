//! The big per-mode entry builder for the command palette.
//!
//! `build_palette_entries` walks `app.palette.mode` and emits the
//! list of `egui_palette::Entry`s the user sees. Each mode owns one
//! arm of the match. New palette modes / commands plug in here.

use hxy_vfs::TabSource;
use hxy_vfs::VfsHandler;

use crate::app::HxyApp;
use crate::commands::palette::Action;
use crate::commands::palette::Mode;
use crate::commands::palette::PaletteCommand;
use crate::commands::shortcuts::COPY_BYTES;
use crate::commands::shortcuts::COPY_HEX;
use crate::commands::shortcuts::FOCUS_PANE;
use crate::commands::shortcuts::NEW_FILE;
use crate::commands::shortcuts::PASTE;
use crate::commands::shortcuts::PASTE_AS_HEX;
use crate::commands::shortcuts::REDO;
use crate::commands::shortcuts::REOPEN_CLOSED_TAB;
use crate::commands::shortcuts::TOGGLE_EDIT_MODE;
use crate::commands::shortcuts::UNDO;
use crate::files::copy::CopyKind;
use crate::tabs::Tab;

use super::offset::OffsetPaletteContext;

/// Snapshot of the active selection used by the palette to decide
/// which `Copy as...` entries to expose. `None` when no file is
/// focused or the selection is empty.
#[derive(Clone, Copy)]
pub struct CopyPaletteContext {
    /// True when the selection width is a scalar integer width
    /// (1/2/4/8 bytes), meaning the `Copy value as...` options apply.
    pub scalar_width: bool,
}

#[derive(Clone, Copy, Default)]
pub struct HistoryPaletteContext {
    pub can_undo: bool,
    pub can_redo: bool,
    /// True when the active tab is mutable and would accept a paste.
    pub can_paste: bool,
    /// True when an active file tab exists, regardless of edit mode.
    /// Gates toggle-read-only and other tab-level actions.
    pub has_active_file: bool,
    /// True when the active file has a detected VFS handler -- gates
    /// the "Browse VFS" entry. Disabled (greyed) when false so the
    /// entry stays discoverable even on plain files.
    pub can_browse_vfs: bool,
    /// True when at least one workspace exists in the dock. Gates
    /// the "Toggle VFS panel" entry, which is meaningless without a
    /// workspace to toggle in.
    pub has_workspace: bool,
    /// True when the active file has a filesystem-backed root
    /// path -- the "Reload file..." entry needs one to read
    /// fresh bytes from. False for in-memory scratch buffers
    /// and plugin-mount tabs.
    pub has_disk_source: bool,
    /// Effective auto-reload mode for the active file. Used by
    /// the watch palette entries to mark the currently-active
    /// option (`*`) so the user can see at a glance which mode
    /// is in effect for this tab.
    pub watch_mode: Option<crate::settings::AutoReloadMode>,
    /// Snapshot of the active file's running template. `None` when no
    /// template has been run for that tab. Carries enough info to
    /// gate / preview template-relative entries (next-field jump,
    /// etc.) without re-borrowing `app.files`.
    pub template: Option<TemplateCtx>,
    /// True when the active file has at least one
    /// `[[hex::visualize(...)]]` target on its parsed templates --
    /// gates the "Show Visualizer" palette entry so it stays out of
    /// sight on files where it would do nothing.
    pub has_visualizer: bool,
    /// FileId of the active tab when one is focused. Held on the
    /// context so `build_palette_entries` (which only borrows the
    /// app immutably) can route per-file commands without needing
    /// `&mut HxyApp` to look up the focused tab.
    pub active_file_id: Option<crate::files::FileId>,
}

#[derive(Clone, Copy, Default)]
pub struct TemplateCtx {
    /// Number of leaf fields the template emitted. Zero when the
    /// template ran but produced no fields (e.g. parse-fail diag-only
    /// state). The next/previous-field entries enable themselves only
    /// when this is non-zero.
    pub field_count: usize,
}

/// Snapshot of the active tab used for ranking `Run Template`
/// entries against its content. Empty when no file is active --
/// `rank_entries` falls through to the default ordering in that
/// case.
#[derive(Clone, Default)]
pub struct TemplatePaletteContext {
    pub extension: Option<String>,
    pub head_bytes: Vec<u8>,
    /// Current non-empty selection on the active file, if any.
    /// Surfaced in the Templates mode so each "Run X" entry can offer
    /// a sibling "Run X at selection" variant. Carried separately
    /// from `CopyPaletteContext` because the templates mode needs it
    /// even when the copy palette doesn't (different mode).
    pub selection: Option<hxy_core::ByteRange>,
}

pub fn copy_palette_context(app: &mut HxyApp) -> Option<CopyPaletteContext> {
    let id = crate::app::active_file_id(app)?;
    let file = app.files.get(&id)?;
    let sel = file.editor.selection()?;
    let range = sel.range();
    if range.is_empty() {
        return None;
    }
    Some(CopyPaletteContext { scalar_width: matches!(range.len().get(), 1 | 2 | 4 | 8) })
}

pub fn history_palette_context(app: &mut HxyApp) -> HistoryPaletteContext {
    let has_workspace = !app.workspaces.is_empty();
    let Some(id) = crate::app::active_file_id(app) else {
        return HistoryPaletteContext { has_workspace, ..HistoryPaletteContext::default() };
    };
    let Some(file) = app.files.get(&id) else {
        return HistoryPaletteContext { has_workspace, ..HistoryPaletteContext::default() };
    };
    let watch_key = app.watch_key_for(id);
    let watch_mode = watch_key.as_ref().map(|k| app.state.read().app.auto_reload_for(k));
    HistoryPaletteContext {
        can_undo: file.editor.can_undo(),
        can_redo: file.editor.can_redo(),
        can_paste: file.editor.edit_mode() == crate::files::EditMode::Mutable,
        has_active_file: true,
        can_browse_vfs: file.detected_handler.is_some(),
        has_workspace,
        has_disk_source: file.root_path().is_some(),
        watch_mode,
        template: file.active_template().map(|t| TemplateCtx { field_count: t.state.leaf_boundaries.len() }),
        has_visualizer: !crate::visualizers::collect_targets(file).is_empty(),
        active_file_id: Some(id),
    }
}

pub fn template_palette_context(app: &mut HxyApp) -> TemplatePaletteContext {
    let Some(id) = crate::app::active_file_id(app) else { return TemplatePaletteContext::default() };
    let Some(file) = app.files.get(&id) else { return TemplatePaletteContext::default() };
    let extension = file.source_kind.as_ref().and_then(|s| s.leaf_extension());
    let source_len = file.editor.source().len().get();
    let window = source_len.min(crate::templates::library::DETECTION_WINDOW as u64);
    // Read failure here just means the ranker has no magic bytes to
    // match against; fall through to the default ordering rather than
    // showing the user an error for a benign read miss in the palette.
    let head_bytes = match hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(window)) {
        Ok(range) if window > 0 => file.editor.source().read(range).unwrap_or_default(),
        _ => Vec::new(),
    };
    let selection = file.editor.selection().and_then(|s| {
        let r = s.range();
        if r.is_empty() { None } else { Some(r) }
    });
    TemplatePaletteContext { extension, head_bytes, selection }
}

/// Build the single argument-style row for the
/// [`Mode::SetPollInterval`] mode: parses `query` as a
/// non-negative integer of milliseconds, clamps it to the
/// supported window, and emits an [`Action::SetPollInterval`].
/// `0` is preserved as the "disable polling" sentinel and
/// labeled distinctly so the user knows what they're picking.
fn build_poll_interval_entries(
    out: &mut Vec<egui_palette::Entry<Action>>,
    query: &str,
    resolver: &dyn hxy_calculator::PathResolver,
) {
    use crate::files::watch::PollingPrefs;
    use egui_phosphor::regular as icon;
    if query.is_empty() {
        return;
    }
    let parsed = match crate::commands::goto::parse_count_expr(query, resolver) {
        Ok(n) => n,
        Err(e) => {
            invalid_entry(out, query, &e.to_string());
            return;
        }
    };
    let ms = match u32::try_from(parsed) {
        Ok(n) => n,
        Err(_) => {
            invalid_entry(out, query, "interval too large");
            return;
        }
    };
    let entry = if ms == 0 {
        egui_palette::Entry::new(hxy_i18n::t("palette-set-poll-interval-off"), Action::SetPollInterval(0))
            .with_icon(icon::TIMER)
    } else {
        let min_ms = PollingPrefs::MIN_INTERVAL.as_millis() as u32;
        let max_ms = PollingPrefs::MAX_INTERVAL.as_millis() as u32;
        let clamped = ms.clamp(min_ms, max_ms);
        let label = if clamped == ms {
            hxy_i18n::t_args("palette-set-poll-interval-fmt", &[("ms", &ms.to_string())])
        } else {
            hxy_i18n::t_args(
                "palette-set-poll-interval-clamped",
                &[("ms", &ms.to_string()), ("clamped", &clamped.to_string())],
            )
        };
        egui_palette::Entry::new(label, Action::SetPollInterval(clamped)).with_icon(icon::TIMER)
    };
    out.push(entry);
}

/// Push a non-actionable "Invalid: {reason}" row. Activating it
/// falls through to `apply_palette_action`'s existing Invalid arm
/// which just closes the palette -- keeps a visible indication
/// that the query isn't parseable without silently showing an
/// empty list.
pub use super::invalid_entry;

/// Resolve a `@<expression>` query into a single Go-to-offset
/// palette entry. When the expression is empty (`@` with nothing
/// after) we render an inert hint so the user sees the prompt
/// instead of an empty list. Parse / evaluation errors render as
/// a non-actionable "Invalid: ..." row -- consistent with the
/// other argument-style palette modes.
///
/// Template field paths (`png.length`) are resolved through the
/// active file's `templates` slice. Unrecognised template names,
/// missing fields, and non-integer scalars all surface as
/// "Invalid: ..." rows; the user can keep typing and the row
/// updates each frame.
fn build_calculator_entry(
    out: &mut Vec<egui_palette::Entry<Action>>,
    expr: &str,
    offset_ctx: &OffsetPaletteContext,
    resolver: &dyn hxy_calculator::PathResolver,
) {
    use egui_phosphor::regular as icon;

    let trimmed = expr.trim();
    if trimmed.is_empty() {
        out.push(
            egui_palette::Entry::new(hxy_i18n::t("palette-go-to-offset-prompt"), Action::NoOp)
                .with_icon(icon::CALCULATOR),
        );
        return;
    }
    if !offset_ctx.available {
        invalid_entry(out, trimmed, &hxy_i18n::t("palette-invalid-no-active-file"));
        return;
    }
    let value = match hxy_calculator::evaluate_str_with(trimmed, resolver) {
        Ok(v) => v,
        Err(e) => {
            invalid_entry(out, trimmed, &e.to_string());
            return;
        }
    };
    let raw = value.raw();
    let max_offset = offset_ctx.source_len.saturating_sub(1);
    let target = match value.as_u64() {
        Ok(t) if t <= max_offset => t,
        Ok(t) => {
            invalid_entry(
                out,
                trimmed,
                &hxy_i18n::t_args(
                    "palette-calculator-out-of-range",
                    &[("value", &format!("0x{t:X}")), ("max", &format!("0x{max_offset:X}"))],
                ),
            );
            return;
        }
        Err(e) => {
            invalid_entry(out, trimmed, &e.to_string());
            return;
        }
    };
    out.push(
        egui_palette::Entry::new(
            hxy_i18n::t_args("palette-go-to-offset-fmt", &[("offset", &format!("0x{target:X}"))]),
            Action::GoToOffset(target),
        )
        .with_icon(icon::CALCULATOR)
        .with_subtitle(format!("{trimmed} = {raw}")),
    );
}

/// Resolve a `=<expression>` query into "Copy result" entries.
/// Mirrors [`build_calculator_entry`] but emits *two* rows --
/// decimal and hex -- so the user can pick the format that
/// matches whatever they're pasting into. Both rows route
/// through the same [`Action::CopyText`] dispatch.
fn build_calculator_copy_entries(
    out: &mut Vec<egui_palette::Entry<Action>>,
    expr: &str,
    resolver: &dyn hxy_calculator::PathResolver,
) {
    use egui_phosphor::regular as icon;

    let trimmed = expr.trim();
    if trimmed.is_empty() {
        out.push(
            egui_palette::Entry::new(hxy_i18n::t("palette-copy-result-prompt"), Action::NoOp)
                .with_icon(icon::CALCULATOR),
        );
        return;
    }
    let value = match hxy_calculator::evaluate_str_with(trimmed, resolver) {
        Ok(v) => v,
        Err(e) => {
            invalid_entry(out, trimmed, &e.to_string());
            return;
        }
    };
    let raw = value.raw();
    let decimal = format!("{raw}");
    let hex = format_signed_hex(raw);
    out.push(
        egui_palette::Entry::new(
            hxy_i18n::t_args("palette-copy-decimal-fmt", &[("value", &decimal)]),
            Action::CopyText(decimal.clone()),
        )
        .with_icon(icon::COPY)
        .with_subtitle(hex.clone()),
    );
    out.push(
        egui_palette::Entry::new(hxy_i18n::t_args("palette-copy-hex-fmt", &[("value", &hex)]), Action::CopyText(hex))
            .with_icon(icon::COPY)
            .with_subtitle(decimal),
    );
}

/// Format a signed `i128` as a `0x...` literal. Negative values
/// get a leading `-` rather than a two's-complement bit pattern;
/// `-16` renders `-0x10`, not a 128-bit value with the high bits
/// set. Most paste targets (debuggers, hex editors, code) expect
/// the signed-magnitude form.
fn format_signed_hex(value: i128) -> String {
    if value < 0 { format!("-0x{:X}", value.unsigned_abs()) } else { format!("0x{value:X}") }
}

/// Build a QuickOpen palette entry for a tool / panel tab kind.
/// Returns `None` for `Tab::File` and `Tab::Workspace` because the
/// surrounding QuickOpen loop already lists every `OpenFile` (which
/// covers both plain file tabs and workspace-nested editor / entry
/// sub-tabs) via [`Action::FocusFile`]. Adding the outer-dock entry
/// for those would duplicate rows.
fn quick_open_entry_for_tab(app: &HxyApp, tab: Tab) -> Option<egui_palette::Entry<Action>> {
    use egui_phosphor::regular as icon;

    let (title, icon_glyph): (String, &'static str) = match tab {
        Tab::File(_) | Tab::Workspace(_) => return None,
        Tab::Welcome => (hxy_i18n::t("tab-welcome"), icon::HOUSE),
        Tab::Settings => (hxy_i18n::t("tab-settings"), icon::GEAR),
        Tab::Console => (hxy_i18n::t("tab-console"), icon::TERMINAL),
        Tab::Inspector => (hxy_i18n::t("tab-inspector"), icon::EYE),
        Tab::Plugins => (hxy_i18n::t("tab-plugins"), icon::PUZZLE_PIECE),
        Tab::Memory => (hxy_i18n::t("tab-memory"), icon::CHART_BAR),
        Tab::SearchResults => (hxy_i18n::t("tab-search-results"), icon::MAGNIFYING_GLASS),
        Tab::Entropy(file_id) => {
            let name = app.files.get(&file_id).map(|f| f.display_name.as_str()).unwrap_or("");
            (hxy_i18n::t_args("tab-entropy", &[("name", name)]), icon::CHART_LINE)
        }
        Tab::Visualizer(file_id) => {
            let name = app.files.get(&file_id).map(|f| f.display_name.as_str()).unwrap_or("");
            (hxy_i18n::t_args("tab-visualizer", &[("name", name)]), icon::SHAPES)
        }
        Tab::Strings(file_id) => {
            let name = app.files.get(&file_id).map(|f| f.display_name.as_str()).unwrap_or("");
            (hxy_i18n::t_args("tab-strings", &[("name", name)]), icon::TEXT_T)
        }
        Tab::Checksums(file_id) => {
            let name = app.files.get(&file_id).map(|f| f.display_name.as_str()).unwrap_or("");
            (hxy_i18n::t_args("tab-checksums", &[("name", name)]), icon::FINGERPRINT)
        }
        Tab::Compare(compare_id) => match app.compares.get(&compare_id) {
            Some(s) => (
                hxy_i18n::t_args("tab-compare-title", &[("a", &s.a.display_name), ("b", &s.b.display_name)]),
                icon::GIT_DIFF,
            ),
            None => return None,
        },
        Tab::PluginMount(mount_id) => match app.mounts.get(&mount_id) {
            Some(m) => (m.display_name.clone(), icon::TREE_STRUCTURE),
            None => return None,
        },
    };
    Some(egui_palette::Entry::new(title, Action::FocusTab(tab)).with_icon(icon_glyph))
}

pub fn build_palette_entries(
    ctx: &egui::Context,
    app: &HxyApp,
    copy_ctx: Option<CopyPaletteContext>,
    history_ctx: HistoryPaletteContext,
    template_ctx: &TemplatePaletteContext,
    offset_ctx: &OffsetPaletteContext,
) -> Vec<egui_palette::Entry<Action>> {
    use egui_phosphor::regular as icon;

    let fmt = |sc: &egui::KeyboardShortcut| ctx.format_shortcut(sc);
    let mut out: Vec<egui_palette::Entry<Action>> = Vec::new();
    // Calculator-resolver scoped to the active file's templates.
    // Cheap to construct (just borrows the slice); used by every
    // mode that accepts an expression. When no file is active or
    // the file has no templates, the resolver still handles plain
    // arithmetic / units -- only path lookups error.
    let calc_resolver = {
        let templates: &[crate::files::TemplateInstance] =
            app.last_active_file.and_then(|id| app.files.get(&id)).map(|f| f.templates.as_slice()).unwrap_or(&[]);
        super::calculator::TemplateFieldResolver::new(templates)
    };
    match app.palette.mode {
        Mode::Main => {
            // `@<expression>` is a calculator-driven Go to Offset
            // shortcut: the rest of the query is parsed as an
            // arithmetic expression and the result is offered as a
            // single entry. Pressing Enter jumps directly without
            // entering the GoToOffset sub-mode. When `@` is present
            // we stop building the standard Main list -- the user
            // committed to the expression flow, and showing a fuzzy-
            // matched grab bag of unrelated commands underneath
            // would be noise.
            if let Some(rest) = app.palette.inner.query.trim_start().strip_prefix('@') {
                build_calculator_entry(&mut out, rest, offset_ctx, &calc_resolver);
                return out;
            }
            if let Some(rest) = app.palette.inner.query.trim_start().strip_prefix('=') {
                build_calculator_copy_entries(&mut out, rest, &calc_resolver);
                return out;
            }
            out.push(
                egui_palette::Entry::new(hxy_i18n::t("menu-file-new"), Action::InvokeCommand(PaletteCommand::NewFile))
                    .with_icon(icon::FILE_PLUS)
                    .with_shortcut(fmt(&NEW_FILE)),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("toolbar-open-file"),
                    Action::InvokeCommand(PaletteCommand::OpenFile),
                )
                .with_icon(icon::FOLDER_OPEN),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-open-file-with-options"),
                    Action::InvokeCommand(PaletteCommand::OpenFileWithOptions),
                )
                .with_icon(icon::FOLDER_OPEN)
                .with_subtitle(hxy_i18n::t("palette-open-file-with-options-subtitle")),
            );
            if !app.state.read().app.recent_files.is_empty() {
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-open-recent-entry"),
                        Action::SwitchMode(Mode::Recent),
                    )
                    .with_icon(icon::CLOCK_COUNTER_CLOCKWISE),
                );
            }
            // Reopen-last-closed-tab: surfaced only when the in-memory
            // ring buffer is non-empty so the row never sits inert.
            // Subtitle previews the most recent capture so the user
            // can tell which tab Cmd+Shift+T is about to bring back.
            if let Some(last) = app.closed_tabs.back() {
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-reopen-closed-tab"),
                        Action::InvokeCommand(PaletteCommand::ReopenClosedTab),
                    )
                    .with_icon(icon::ARROW_U_UP_LEFT)
                    .with_subtitle(last.display_name.clone())
                    .with_shortcut(fmt(&REOPEN_CLOSED_TAB)),
                );
            }
            // BrowseVfs only does something when the active file has
            // a detected VFS handler. Surface the entry either way so
            // the user can find it, but disable it (and add a
            // "no handler for this file" subtitle) when it'd no-op.
            let mut browse_vfs = egui_palette::Entry::new(
                hxy_i18n::t("toolbar-browse-vfs"),
                Action::InvokeCommand(PaletteCommand::BrowseVfs),
            )
            .with_icon(icon::TREE_STRUCTURE)
            .with_disabled(!history_ctx.can_browse_vfs);
            if !history_ctx.can_browse_vfs {
                browse_vfs = browse_vfs.with_subtitle(hxy_i18n::t("palette-browse-vfs-unavailable"));
            }
            out.push(browse_vfs);
            // Per-tool entries use dynamic titles -- "Show X"
            // when the panel is hidden, "Close X" when it's
            // visible -- so the palette row literally tells
            // the user what pressing Enter will do. No
            // subtitle: the title carries the action verb,
            // and a "Hide / Show" subtitle would just repeat
            // the same word in two places.
            let console_visible = app.dock.find_tab(&Tab::Console).is_some();
            let inspector_visible = app.dock.find_tab(&Tab::Inspector).is_some();
            let plugins_visible = app.dock.find_tab(&Tab::Plugins).is_some();
            let entropy_visible =
                history_ctx.has_active_file && app.dock.iter_all_tabs().any(|(_, t)| matches!(t, Tab::Entropy(_)));
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t(if console_visible {
                        "palette-tool-close-console"
                    } else {
                        "palette-tool-show-console"
                    }),
                    Action::InvokeCommand(PaletteCommand::ToggleConsole),
                )
                .with_icon(icon::TERMINAL),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t(if inspector_visible {
                        "palette-tool-close-inspector"
                    } else {
                        "palette-tool-show-inspector"
                    }),
                    Action::InvokeCommand(PaletteCommand::ToggleInspector),
                )
                .with_icon(icon::EYE),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t(if plugins_visible {
                        "palette-tool-close-plugins"
                    } else {
                        "palette-tool-show-plugins"
                    }),
                    Action::InvokeCommand(PaletteCommand::TogglePlugins),
                )
                .with_icon(icon::PUZZLE_PIECE),
            );
            let settings_visible = app.dock.find_tab(&Tab::Settings).is_some();
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t(if settings_visible {
                        "palette-tool-close-settings"
                    } else {
                        "palette-tool-show-settings"
                    }),
                    Action::InvokeCommand(PaletteCommand::ToggleSettings),
                )
                .with_icon(icon::GEAR),
            );
            let memory_visible = app.dock.find_tab(&Tab::Memory).is_some();
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t(if memory_visible { "palette-tool-close-memory" } else { "palette-tool-show-memory" }),
                    Action::InvokeCommand(PaletteCommand::ToggleMemory),
                )
                .with_icon(icon::CHART_BAR),
            );
            if history_ctx.has_active_file {
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t(if entropy_visible {
                            "palette-tool-close-entropy"
                        } else {
                            "palette-tool-show-entropy"
                        }),
                        Action::InvokeCommand(PaletteCommand::ToggleEntropy),
                    )
                    .with_icon(icon::CHART_LINE),
                );
            }

            // Visualizer entry only shows when the active file's
            // parsed templates contain visualizer-bearing fields. The
            // panel stays closed by default, so this is the primary
            // path for popping it the first time.
            if history_ctx.has_visualizer
                && let Some(active_id) = history_ctx.active_file_id
            {
                let visualizer_visible = app.dock.find_tab(&Tab::Visualizer(active_id)).is_some();
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t(if visualizer_visible {
                            "palette-tool-close-visualizer"
                        } else {
                            "palette-tool-show-visualizer"
                        }),
                        Action::InvokeCommand(PaletteCommand::ToggleVisualizer),
                    )
                    .with_icon(icon::SHAPES),
                );
            }

            // Strings tool: three entries -- whole-file, selection
            // (only when a non-empty selection exists), and a
            // "...with options" entry that opens the tab without
            // auto-running so the user can adjust encoding / min
            // length first.
            if history_ctx.has_active_file {
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-strings-whole-file"),
                        Action::InvokeCommand(PaletteCommand::FindStringsWholeFile),
                    )
                    .with_icon(icon::TEXT_T)
                    .with_subtitle(hxy_i18n::t("palette-strings-whole-file-subtitle")),
                );
                if let Some(sel) = template_ctx.selection {
                    let subtitle = hxy_i18n::t_args(
                        "palette-strings-selection-subtitle",
                        &[("start", &format!("{:#x}", sel.start().get())), ("end", &format!("{:#x}", sel.end().get()))],
                    );
                    out.push(
                        egui_palette::Entry::new(
                            hxy_i18n::t("palette-strings-selection"),
                            Action::InvokeCommand(PaletteCommand::FindStringsSelection),
                        )
                        .with_icon(icon::TEXT_T)
                        .with_subtitle(subtitle),
                    );
                }
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-strings-with-options"),
                        Action::InvokeCommand(PaletteCommand::FindStringsWithOptions),
                    )
                    .with_icon(icon::TEXT_T)
                    .with_subtitle(hxy_i18n::t("palette-strings-with-options-subtitle")),
                );
            }

            // Checksum tool: whole-file always available; selection
            // entry shows when there's a non-empty selection.
            if history_ctx.has_active_file {
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-checksums-whole-file"),
                        Action::InvokeCommand(PaletteCommand::CalculateChecksumsWholeFile),
                    )
                    .with_icon(icon::FINGERPRINT)
                    .with_subtitle(hxy_i18n::t("palette-checksums-whole-file-subtitle")),
                );
                if let Some(sel) = template_ctx.selection {
                    let subtitle = hxy_i18n::t_args(
                        "palette-checksums-selection-subtitle",
                        &[("start", &format!("{:#x}", sel.start().get())), ("end", &format!("{:#x}", sel.end().get()))],
                    );
                    out.push(
                        egui_palette::Entry::new(
                            hxy_i18n::t("palette-checksums-selection"),
                            Action::InvokeCommand(PaletteCommand::CalculateChecksumsSelection),
                        )
                        .with_icon(icon::FINGERPRINT)
                        .with_subtitle(subtitle),
                    );
                }
            }

            // Skip the "Toggle VFS panel" entry entirely when no
            // workspace is open -- it'd toggle nothing in that case
            // and just clutters the list.
            if history_ctx.has_workspace {
                let workspace_tree_visible = app
                    .dock
                    .focused_leaf()
                    .and_then(|p| app.dock.leaf(p).ok())
                    .and_then(|leaf| leaf.tabs().get(leaf.active.0))
                    .and_then(|tab| match tab {
                        Tab::Workspace(workspace_id) => app
                            .workspaces
                            .get(workspace_id)
                            .map(|w| w.dock.find_tab(&crate::files::WorkspaceTab::VfsTree).is_some()),
                        _ => None,
                    })
                    .unwrap_or(false);
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t(if workspace_tree_visible {
                            "palette-tool-close-vfs"
                        } else {
                            "palette-tool-show-vfs"
                        }),
                        Action::InvokeCommand(PaletteCommand::ToggleWorkspaceVfs),
                    )
                    .with_icon(icon::TREE_STRUCTURE),
                );
            }

            // Bulk close: only meaningful when at least one
            // tool-only leaf exists. The action auto-picks the
            // single candidate when there's just one and falls
            // back to the visual pane picker for multiple, so
            // the entry's behaviour is "close one tool pane;
            // ask which if it's ambiguous".
            let tool_only_leaves = crate::tabs::dock_ops::tool_only_leaves(&app.dock);
            if !tool_only_leaves.is_empty() {
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-close-tool-pane"),
                        Action::InvokeCommand(PaletteCommand::CloseToolPane),
                    )
                    .with_icon(icon::SQUARES_FOUR),
                );
            }
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-run-template-entry"),
                    Action::SwitchMode(Mode::Templates),
                )
                .with_icon(icon::SCROLL),
            );
            if let Some(sel) = template_ctx.selection {
                let subtitle = hxy_i18n::t_args(
                    "palette-run-template-at-selection-subtitle",
                    &[("start", &format!("{:#x}", sel.start().get())), ("end", &format!("{:#x}", sel.end().get()))],
                );
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-run-template-at-selection-entry"),
                        Action::SwitchMode(Mode::TemplatesAtSelection),
                    )
                    .with_icon(icon::SCROLL)
                    .with_subtitle(subtitle),
                );
            }
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-uninstall-template"),
                    Action::SwitchMode(Mode::Uninstall),
                )
                .with_icon(icon::TRASH),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-uninstall-plugin"),
                    Action::SwitchMode(Mode::UninstallPlugin),
                )
                .with_subtitle(hxy_i18n::t("palette-delete-plugin-subtitle"))
                .with_icon(icon::TRASH),
            );
            if history_ctx.has_active_file {
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-go-to-offset-entry"),
                        Action::SwitchMode(Mode::GoToOffset),
                    )
                    .with_subtitle(hxy_i18n::t("palette-go-to-offset-shortcut-hint"))
                    .with_icon(icon::CROSSHAIR),
                );
                if offset_ctx.virtual_base.is_some() {
                    out.push(
                        egui_palette::Entry::new(
                            hxy_i18n::t("palette-go-to-address-entry"),
                            Action::SwitchMode(Mode::GoToAddress),
                        )
                        .with_subtitle(hxy_i18n::t("palette-go-to-address-shortcut-hint"))
                        .with_icon(icon::CROSSHAIR),
                    );
                }
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-select-from-offset-entry"),
                        Action::SwitchMode(Mode::SelectFromOffset),
                    )
                    .with_icon(icon::ARROWS_OUT_LINE_HORIZONTAL),
                );
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-select-range-entry"),
                        Action::SwitchMode(Mode::SelectRange),
                    )
                    .with_icon(icon::BRACKETS_CURLY),
                );
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-set-columns-local-entry"),
                        Action::SwitchMode(Mode::SetColumnsLocal),
                    )
                    .with_icon(icon::COLUMNS),
                );
                let vbase_label = match offset_ctx.virtual_base {
                    Some(addr) => hxy_i18n::t_args(
                        "palette-set-virtual-base-entry-current",
                        &[("address", &format!("0x{addr:X}"))],
                    ),
                    None => hxy_i18n::t("palette-set-virtual-base-entry"),
                };
                out.push(
                    egui_palette::Entry::new(vbase_label, Action::SwitchMode(Mode::SetVirtualBase))
                        .with_icon(icon::TARGET),
                );
                let has_fields = history_ctx.template.is_some_and(|t| t.field_count > 0);
                let mut next_field = egui_palette::Entry::new(
                    hxy_i18n::t("palette-jump-next-field"),
                    Action::InvokeCommand(PaletteCommand::JumpNextField),
                )
                .with_icon(icon::ARROW_RIGHT)
                .with_shortcut(fmt(&crate::commands::shortcuts::JUMP_NEXT_FIELD))
                .with_disabled(!has_fields);
                let mut prev_field = egui_palette::Entry::new(
                    hxy_i18n::t("palette-jump-prev-field"),
                    Action::InvokeCommand(PaletteCommand::JumpPrevField),
                )
                .with_icon(icon::ARROW_LEFT)
                .with_shortcut(fmt(&crate::commands::shortcuts::JUMP_PREV_FIELD))
                .with_disabled(!has_fields);
                if !has_fields {
                    next_field = next_field.with_subtitle(hxy_i18n::t("palette-jump-field-no-template"));
                    prev_field = prev_field.with_subtitle(hxy_i18n::t("palette-jump-field-no-template"));
                }
                out.push(next_field);
                out.push(prev_field);
            }
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-set-columns-global-entry"),
                    Action::SwitchMode(Mode::SetColumnsGlobal),
                )
                .with_icon(icon::COLUMNS_PLUS_RIGHT),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-set-poll-interval-entry"),
                    Action::SwitchMode(Mode::SetPollInterval),
                )
                .with_icon(icon::TIMER)
                .with_subtitle(hxy_i18n::t_args(
                    "palette-set-poll-interval-current",
                    &[("ms", &app.state.read().app.file_poll_interval_ms.to_string())],
                )),
            );
            if history_ctx.can_undo {
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("menu-edit-undo"),
                        Action::InvokeCommand(PaletteCommand::Undo),
                    )
                    .with_icon(icon::ARROW_COUNTER_CLOCKWISE)
                    .with_shortcut(fmt(&UNDO)),
                );
            }
            if history_ctx.can_redo {
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("menu-edit-redo"),
                        Action::InvokeCommand(PaletteCommand::Redo),
                    )
                    .with_icon(icon::ARROW_CLOCKWISE)
                    .with_shortcut(fmt(&REDO)),
                );
            }
            if history_ctx.has_active_file {
                let (result_key, toggle_icon) = if history_ctx.can_paste {
                    ("palette-toggle-readonly-result-readonly", icon::LOCK)
                } else {
                    ("palette-toggle-readonly-result-mutable", icon::LOCK_OPEN)
                };
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-toggle-readonly"),
                        Action::InvokeCommand(PaletteCommand::ToggleEditMode),
                    )
                    .with_subtitle(hxy_i18n::t(result_key))
                    .with_icon(toggle_icon)
                    .with_shortcut(fmt(&TOGGLE_EDIT_MODE)),
                );
                let mut reload_entry = egui_palette::Entry::new(
                    hxy_i18n::t("palette-reload-file"),
                    Action::InvokeCommand(PaletteCommand::ReloadActiveFile),
                )
                .with_icon(icon::ARROWS_CLOCKWISE)
                .with_subtitle(hxy_i18n::t("palette-reload-file-subtitle"))
                .with_disabled(!history_ctx.has_disk_source);
                if !history_ctx.has_disk_source {
                    reload_entry = reload_entry.with_subtitle(hxy_i18n::t("palette-reload-no-disk-source"));
                }
                out.push(reload_entry);

                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-take-snapshot"),
                        Action::InvokeCommand(PaletteCommand::TakeSnapshot),
                    )
                    .with_icon(icon::CAMERA)
                    .with_subtitle(hxy_i18n::t("palette-take-snapshot-subtitle")),
                );
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-open-snapshots"),
                        Action::InvokeCommand(PaletteCommand::OpenSnapshots),
                    )
                    .with_icon(icon::IMAGES)
                    .with_subtitle(hxy_i18n::t("palette-open-snapshots-subtitle")),
                );

                let active_subtitle = |label_key: &str, marker: &str| -> String {
                    hxy_i18n::t_args("palette-watch-subtitle", &[("mode", &hxy_i18n::t(label_key)), ("marker", marker)])
                };
                let mark_for = |this_mode: crate::settings::AutoReloadMode| -> &'static str {
                    if Some(this_mode) == history_ctx.watch_mode { "*" } else { "" }
                };
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-watch-always"),
                        Action::InvokeCommand(PaletteCommand::WatchAlways),
                    )
                    .with_icon(icon::EYE)
                    .with_subtitle(active_subtitle(
                        crate::settings::AutoReloadMode::Always.label_key(),
                        mark_for(crate::settings::AutoReloadMode::Always),
                    )),
                );
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-watch-ask"),
                        Action::InvokeCommand(PaletteCommand::WatchAsk),
                    )
                    .with_icon(icon::EYE)
                    .with_subtitle(active_subtitle(
                        crate::settings::AutoReloadMode::Ask.label_key(),
                        mark_for(crate::settings::AutoReloadMode::Ask),
                    )),
                );
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-watch-never"),
                        Action::InvokeCommand(PaletteCommand::WatchNever),
                    )
                    .with_icon(icon::EYE_SLASH)
                    .with_subtitle(active_subtitle(
                        crate::settings::AutoReloadMode::Never.label_key(),
                        mark_for(crate::settings::AutoReloadMode::Never),
                    )),
                );
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-compute-entropy"),
                        Action::InvokeCommand(PaletteCommand::ComputeEntropy),
                    )
                    .with_icon(icon::CHART_LINE)
                    .with_subtitle(hxy_i18n::t("palette-compute-entropy-subtitle")),
                );
                // The "show entropy panel" use case is now
                // covered by the unified Toggle tool pane
                // entry (subtitle "Entropy - Show / Hide"),
                // so a dedicated show-only row would just be
                // a fuzzy-search dupe.
            }
            if history_ctx.can_paste {
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("menu-edit-paste"),
                        Action::InvokeCommand(PaletteCommand::Paste),
                    )
                    .with_icon(icon::CLIPBOARD_TEXT)
                    .with_shortcut(fmt(&PASTE)),
                );
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("menu-edit-paste-as-hex"),
                        Action::InvokeCommand(PaletteCommand::PasteAsHex),
                    )
                    .with_icon(icon::CLIPBOARD_TEXT)
                    .with_shortcut(fmt(&PASTE_AS_HEX)),
                );
            }
            if history_ctx.has_active_file {
                let base = app.state.read().app.offset_base;
                if let Some((start, end_exclusive)) = offset_ctx.selection {
                    let last_inclusive = end_exclusive.saturating_sub(1);
                    let len = end_exclusive.saturating_sub(start);
                    let caret_preview = crate::view::format::format_offset(offset_ctx.cursor, base);
                    out.push(
                        egui_palette::Entry::new(
                            hxy_i18n::t("palette-copy-caret-offset"),
                            Action::InvokeCommand(PaletteCommand::CopyCaretOffset),
                        )
                        .with_icon(icon::COPY)
                        .with_subtitle(caret_preview),
                    );
                    if let Some(vbase) = offset_ctx.virtual_base {
                        let caret_address_preview =
                            crate::view::format::format_offset_with_vaddr(offset_ctx.cursor, base, vbase);
                        out.push(
                            egui_palette::Entry::new(
                                hxy_i18n::t("palette-copy-caret-address"),
                                Action::InvokeCommand(PaletteCommand::CopyCaretAddress),
                            )
                            .with_icon(icon::COPY)
                            .with_subtitle(caret_address_preview),
                        );
                    }
                    if len > 1 {
                        let len_preview = crate::view::format::format_offset(len, base);
                        let range_preview = format!(
                            "{}-{} ({} bytes)",
                            crate::view::format::format_offset(start, base),
                            crate::view::format::format_offset(last_inclusive, base),
                            len_preview,
                        );
                        out.push(
                            egui_palette::Entry::new(
                                hxy_i18n::t("palette-copy-selection-range"),
                                Action::InvokeCommand(PaletteCommand::CopySelectionRange),
                            )
                            .with_icon(icon::COPY)
                            .with_subtitle(range_preview),
                        );
                        if let Some(vbase) = offset_ctx.virtual_base {
                            let range_address_preview = format!(
                                "{}-{} ({} bytes)",
                                crate::view::format::format_offset_with_vaddr(start, base, vbase),
                                crate::view::format::format_offset_with_vaddr(last_inclusive, base, vbase),
                                len_preview,
                            );
                            out.push(
                                egui_palette::Entry::new(
                                    hxy_i18n::t("palette-copy-selection-range-address"),
                                    Action::InvokeCommand(PaletteCommand::CopySelectionRangeAddress),
                                )
                                .with_icon(icon::COPY)
                                .with_subtitle(range_address_preview),
                            );
                        }
                        out.push(
                            egui_palette::Entry::new(
                                hxy_i18n::t("palette-copy-selection-length"),
                                Action::InvokeCommand(PaletteCommand::CopySelectionLength),
                            )
                            .with_icon(icon::COPY)
                            .with_subtitle(len_preview),
                        );
                    }
                }
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-copy-file-length"),
                        Action::InvokeCommand(PaletteCommand::CopyFileLength),
                    )
                    .with_icon(icon::COPY)
                    .with_subtitle(crate::view::format::format_offset(offset_ctx.source_len, base)),
                );
            }
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-compare-files"),
                    Action::InvokeCommand(PaletteCommand::CompareFiles),
                )
                .with_icon(icon::COLUMNS)
                .with_subtitle(hxy_i18n::t("palette-compare-files-subtitle")),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-compare-files-dialog"),
                    Action::InvokeCommand(PaletteCommand::CompareFilesDialog),
                )
                .with_icon(icon::COLUMNS)
                .with_subtitle(hxy_i18n::t("palette-compare-files-dialog-subtitle")),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-split-right"),
                    Action::InvokeCommand(PaletteCommand::SplitRight),
                )
                .with_icon(icon::ARROW_SQUARE_RIGHT),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-split-left"),
                    Action::InvokeCommand(PaletteCommand::SplitLeft),
                )
                .with_icon(icon::ARROW_SQUARE_LEFT),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-split-down"),
                    Action::InvokeCommand(PaletteCommand::SplitDown),
                )
                .with_icon(icon::ARROW_SQUARE_DOWN),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-split-up"),
                    Action::InvokeCommand(PaletteCommand::SplitUp),
                )
                .with_icon(icon::ARROW_SQUARE_UP),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-merge-right"),
                    Action::InvokeCommand(PaletteCommand::MergeRight),
                )
                .with_icon(icon::ARROW_LINE_RIGHT),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-merge-left"),
                    Action::InvokeCommand(PaletteCommand::MergeLeft),
                )
                .with_icon(icon::ARROW_LINE_LEFT),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-merge-down"),
                    Action::InvokeCommand(PaletteCommand::MergeDown),
                )
                .with_icon(icon::ARROW_LINE_DOWN),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-merge-up"),
                    Action::InvokeCommand(PaletteCommand::MergeUp),
                )
                .with_icon(icon::ARROW_LINE_UP),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-move-tab-right"),
                    Action::InvokeCommand(PaletteCommand::MoveTabRight),
                )
                .with_icon(icon::ARROW_FAT_RIGHT),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-move-tab-left"),
                    Action::InvokeCommand(PaletteCommand::MoveTabLeft),
                )
                .with_icon(icon::ARROW_FAT_LEFT),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-move-tab-down"),
                    Action::InvokeCommand(PaletteCommand::MoveTabDown),
                )
                .with_icon(icon::ARROW_FAT_DOWN),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-move-tab-up"),
                    Action::InvokeCommand(PaletteCommand::MoveTabUp),
                )
                .with_icon(icon::ARROW_FAT_UP),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-move-tab-visual"),
                    Action::InvokeCommand(PaletteCommand::MoveTabVisual),
                )
                .with_icon(icon::CROSSHAIR_SIMPLE)
                .with_subtitle(hxy_i18n::t("palette-pane-pick-subtitle")),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-merge-visual"),
                    Action::InvokeCommand(PaletteCommand::MergeVisual),
                )
                .with_icon(icon::CROSSHAIR_SIMPLE)
                .with_subtitle(hxy_i18n::t("palette-pane-pick-subtitle")),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-focus-pane"),
                    Action::InvokeCommand(PaletteCommand::FocusPane),
                )
                .with_icon(icon::CROSSHAIR_SIMPLE)
                .with_subtitle(hxy_i18n::t("palette-pane-pick-subtitle"))
                .with_shortcut(fmt(&FOCUS_PANE)),
            );
            let vim_active = matches!(app.state.read().app.input_mode, hxy_view::InputMode::Vim);
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-toggle-vim"),
                    Action::InvokeCommand(PaletteCommand::ToggleVim),
                )
                .with_icon(icon::KEYBOARD)
                .with_subtitle(hxy_i18n::t(if vim_active {
                    "palette-toggle-vim-subtitle-on"
                } else {
                    "palette-toggle-vim-subtitle-off"
                })),
            );
            if let Some(copy) = copy_ctx {
                for (label, kind) in crate::files::copy::BYTES_MENU {
                    let mut entry = egui_palette::Entry::new(format!("Copy bytes: {label}"), Action::Copy(*kind))
                        .with_icon(icon::COPY);
                    if matches!(kind, CopyKind::BytesLossyUtf8) {
                        entry = entry.with_shortcut(fmt(&COPY_BYTES));
                    } else if matches!(kind, CopyKind::BytesHexSpaced) {
                        entry = entry.with_shortcut(fmt(&COPY_HEX));
                    }
                    out.push(entry);
                }
                if copy.scalar_width {
                    for (label, kind) in crate::files::copy::VALUE_MENU {
                        out.push(
                            egui_palette::Entry::new(format!("Copy value: {label}"), Action::Copy(*kind))
                                .with_icon(icon::COPY),
                        );
                    }
                }
            }
            for plugin in &app.plugin_handlers {
                let plugin_name = plugin.name().to_owned();
                for cmd in plugin.list_commands() {
                    let mut entry = egui_palette::Entry::new(
                        format!("{plugin_name}: {}", cmd.label),
                        Action::InvokePluginCommand { plugin_name: plugin_name.clone(), command_id: cmd.id },
                    );
                    if let Some(s) = cmd.subtitle {
                        entry = entry.with_subtitle(s);
                    }
                    entry = entry.with_icon(cmd.icon.unwrap_or_else(|| icon::PUZZLE_PIECE.to_string()));
                    out.push(entry);
                }
            }
        }
        Mode::QuickOpen => {
            for (id, file) in &app.files {
                let mut entry =
                    egui_palette::Entry::new(file.display_name.clone(), Action::FocusFile(*id)).with_icon(icon::FILE);
                if let Some(parent) = file.root_path().and_then(|p| p.parent()) {
                    entry = entry.with_subtitle(parent.display().to_string());
                }
                out.push(entry);
            }
            for (_, tab) in app.dock.iter_all_tabs() {
                if let Some(entry) = quick_open_entry_for_tab(app, *tab) {
                    out.push(entry);
                }
            }
            let open_paths: std::collections::HashSet<std::path::PathBuf> =
                app.files.values().filter_map(|f| f.root_path().cloned()).collect();
            for recent in &app.state.read().app.recent_files {
                if open_paths.contains(&recent.path) {
                    continue;
                }
                let name = recent
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| recent.path.display().to_string());
                let mut entry = egui_palette::Entry::new(name, Action::OpenRecent(recent.path.clone()))
                    .with_icon(icon::CLOCK_COUNTER_CLOCKWISE);
                if let Some(parent) = recent.path.parent() {
                    entry = entry.with_subtitle(parent.display().to_string());
                }
                out.push(entry);
            }
        }
        Mode::Templates => {
            let ranked = app.templates.rank_entries(template_ctx.extension.as_deref(), &template_ctx.head_bytes);
            for entry in ranked {
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t_args("palette-run-template-fmt", &[("name", &entry.name)]),
                        Action::RunTemplate { path: entry.path.clone(), range: None },
                    )
                    .with_subtitle(entry.path.display().to_string())
                    .with_icon(icon::SCROLL),
                );
            }
            out.push(
                egui_palette::Entry::new(hxy_i18n::t("palette-install-template"), Action::InstallTemplate)
                    .with_subtitle(hxy_i18n::t("palette-install-template-subtitle"))
                    .with_icon(icon::DOWNLOAD),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-uninstall-template"),
                    Action::SwitchMode(Mode::Uninstall),
                )
                .with_icon(icon::TRASH),
            );
        }
        Mode::TemplatesAtSelection => {
            // Selection might have been cleared between opening this
            // mode and rendering this frame. Bail with an empty list
            // rather than silently degrading to whole-file runs --
            // the user explicitly asked for "at selection" and a
            // missing selection means there's nothing to bind to.
            let Some(sel) = template_ctx.selection else { return out };
            let ranked = app.templates.rank_entries(template_ctx.extension.as_deref(), &template_ctx.head_bytes);
            for entry in ranked {
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t_args("palette-run-template-fmt", &[("name", &entry.name)]),
                        Action::RunTemplate { path: entry.path.clone(), range: Some(sel) },
                    )
                    .with_subtitle(entry.path.display().to_string())
                    .with_icon(icon::SCROLL),
                );
            }
        }
        Mode::Uninstall => {
            if let Some(dir) = crate::app::user_templates_dir() {
                for path in crate::templates::library::list_installed_templates(&dir) {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    out.push(
                        egui_palette::Entry::new(
                            hxy_i18n::t_args("palette-delete-template-fmt", &[("name", &name)]),
                            Action::UninstallTemplate(path.clone()),
                        )
                        .with_subtitle(path.display().to_string())
                        .with_icon(icon::TRASH),
                    );
                }
            }
        }
        Mode::UninstallPlugin => {
            for dir in [crate::app::user_plugins_dir(), crate::app::user_template_plugins_dir()].into_iter().flatten() {
                let Ok(read) = std::fs::read_dir(&dir) else { continue };
                let mut wasms: Vec<std::path::PathBuf> = read
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("wasm"))
                    .collect();
                wasms.sort();
                for path in wasms {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    out.push(
                        egui_palette::Entry::new(
                            hxy_i18n::t_args("palette-delete-plugin-fmt", &[("name", &name)]),
                            Action::UninstallPlugin(path.clone()),
                        )
                        .with_subtitle(path.display().to_string())
                        .with_icon(icon::TRASH),
                    );
                }
            }
        }
        Mode::Recent => {
            let open_paths: std::collections::HashSet<std::path::PathBuf> =
                app.files.values().filter_map(|f| f.root_path().cloned()).collect();
            for recent in &app.state.read().app.recent_files {
                if open_paths.contains(&recent.path) {
                    continue;
                }
                let name = recent
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| recent.path.display().to_string());
                let mut entry = egui_palette::Entry::new(name, Action::OpenRecent(recent.path.clone()))
                    .with_icon(icon::CLOCK_COUNTER_CLOCKWISE);
                if let Some(parent) = recent.path.parent() {
                    entry = entry.with_subtitle(parent.display().to_string());
                }
                out.push(entry);
            }
        }
        Mode::CompareSideA | Mode::CompareSideB => {
            let side = if matches!(app.palette.mode, Mode::CompareSideA) {
                crate::commands::palette::CompareSide::A
            } else {
                crate::commands::palette::CompareSide::B
            };
            let picked_a = app.palette.compare_pick.as_ref().and_then(|p| p.picked_a.clone());
            for file in app.files.values() {
                let Some(source) = file.source_kind.clone() else {
                    continue;
                };
                if matches!(side, crate::commands::palette::CompareSide::B)
                    && picked_a.as_ref().is_some_and(|a| a == &source)
                {
                    continue;
                }
                let mut entry =
                    egui_palette::Entry::new(file.display_name.clone(), Action::CompareSelectSource { side, source })
                        .with_icon(icon::FILE);
                if let Some(parent) = file.root_path().and_then(|p| p.parent()) {
                    entry = entry.with_subtitle(parent.display().to_string());
                }
                out.push(entry);
            }
            let recent_mode = match side {
                crate::commands::palette::CompareSide::A => Mode::CompareSideARecent,
                crate::commands::palette::CompareSide::B => Mode::CompareSideBRecent,
            };
            if !app.state.read().app.recent_files.is_empty() {
                out.push(
                    egui_palette::Entry::new(hxy_i18n::t("palette-open-recent-entry"), Action::SwitchMode(recent_mode))
                        .with_icon(icon::CLOCK_COUNTER_CLOCKWISE),
                );
            }
            out.push(
                egui_palette::Entry::new(hxy_i18n::t("compare-picker-browse"), Action::CompareBrowse(side))
                    .with_icon(icon::FOLDER_OPEN),
            );
        }
        Mode::CompareSideARecent | Mode::CompareSideBRecent => {
            let side = if matches!(app.palette.mode, Mode::CompareSideARecent) {
                crate::commands::palette::CompareSide::A
            } else {
                crate::commands::palette::CompareSide::B
            };
            let picked_a_path =
                app.palette.compare_pick.as_ref().and_then(|p| p.picked_a.as_ref()).and_then(|s| match s {
                    TabSource::Filesystem(p) => Some(p.clone()),
                    _ => None,
                });
            let open_paths: std::collections::HashSet<std::path::PathBuf> =
                app.files.values().filter_map(|f| f.root_path().cloned()).collect();
            for recent in &app.state.read().app.recent_files {
                if open_paths.contains(&recent.path) {
                    continue;
                }
                if matches!(side, crate::commands::palette::CompareSide::B)
                    && picked_a_path.as_ref().is_some_and(|a| a == &recent.path)
                {
                    continue;
                }
                let name = recent
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| recent.path.display().to_string());
                let mut entry = egui_palette::Entry::new(
                    name,
                    Action::CompareSelectSource { side, source: TabSource::Filesystem(recent.path.clone()) },
                )
                .with_icon(icon::CLOCK_COUNTER_CLOCKWISE);
                if let Some(parent) = recent.path.parent() {
                    entry = entry.with_subtitle(parent.display().to_string());
                }
                out.push(entry);
            }
        }
        Mode::GoToOffset | Mode::GoToAddress | Mode::SelectFromOffset | Mode::SelectRange => {
            let query = app.palette.inner.query.trim();
            if !offset_ctx.available {
                invalid_entry(&mut out, query, &hxy_i18n::t("palette-invalid-no-active-file"));
            } else {
                super::offset::build_offset_entries(&mut out, app.palette.mode, query, offset_ctx, &calc_resolver);
            }
        }
        Mode::SetColumnsLocal | Mode::SetColumnsGlobal => {
            let query = app.palette.inner.query.trim();
            if matches!(app.palette.mode, Mode::SetColumnsLocal) && !offset_ctx.available {
                invalid_entry(&mut out, query, &hxy_i18n::t("palette-invalid-no-active-file"));
            } else {
                super::columns::build_columns_entries(&mut out, app.palette.mode, query, &calc_resolver);
            }
        }
        Mode::SetPollInterval => {
            let query = app.palette.inner.query.trim();
            build_poll_interval_entries(&mut out, query, &calc_resolver);
        }
        Mode::SetVirtualBase => {
            let query = app.palette.inner.query.trim();
            if !offset_ctx.available {
                invalid_entry(&mut out, query, &hxy_i18n::t("palette-invalid-no-active-file"));
            } else {
                super::build_virtual_base_entries(&mut out, query, &calc_resolver);
            }
        }
        Mode::PluginCascade => {
            if let Some(cascade) = app.palette.plugin_cascade.as_ref() {
                let plugin_name = &cascade.plugin_name;
                for cmd in &cascade.commands {
                    let mut entry = egui_palette::Entry::new(
                        cmd.label.clone(),
                        Action::InvokePluginCommand { plugin_name: plugin_name.clone(), command_id: cmd.id.clone() },
                    );
                    if let Some(s) = cmd.subtitle.clone() {
                        entry = entry.with_subtitle(s);
                    }
                    entry = entry.with_icon(cmd.icon.clone().unwrap_or_else(|| icon::PUZZLE_PIECE.to_string()));
                    out.push(entry);
                }
            }
        }
        Mode::PluginPrompt => {
            if let Some(prompt) = app.palette.plugin_prompt.as_ref() {
                let answer = app.palette.inner.query.clone();
                let label = if answer.is_empty() { hxy_i18n::t("palette-plugin-prompt-empty") } else { answer.clone() };
                let mut entry = egui_palette::Entry::new(
                    label,
                    Action::RespondToPlugin {
                        plugin_name: prompt.plugin_name.clone(),
                        command_id: prompt.command_id.clone(),
                        answer,
                    },
                )
                .with_icon(icon::ARROW_BEND_DOWN_LEFT);
                entry = entry.with_subtitle(prompt.title.clone());
                out.push(entry);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::format_signed_hex;

    #[test]
    fn signed_hex_positive_zero_negative() {
        assert_eq!(format_signed_hex(0), "0x0");
        assert_eq!(format_signed_hex(0x100), "0x100");
        assert_eq!(format_signed_hex(-16), "-0x10");
        // i128::MIN must not panic on `unsigned_abs` -- it's the
        // one value where naive `abs()` would.
        assert_eq!(format_signed_hex(i128::MIN), format!("-0x{:X}", (i128::MIN as u128).wrapping_neg()));
    }
}
