//! The palette's cascade [`PaletteMode`], its pure-data
//! [`PaletteAction`] vocabulary, and the per-mode entry builders.
//!
//! Everything here is framework-agnostic: [`build_entries`] takes a
//! mode, the raw query, a [`PaletteContext`] snapshot of the active
//! file, and the resolved keybinding hints, and returns the rows the
//! overlay renders. No gpui types leak in, so the builders are unit-
//! tested directly. Argument parsing routes through
//! [`hxy_panels::goto`]; the `@` / `=` calculator prefixes route
//! through [`hxy_calculator`] with a [`NullResolver`] (template field
//! paths arrive in M4).

use hxy_calculator::NullResolver;
use hxy_core::ColumnCount;
use hxy_panels::goto::ParseError;
use hxy_panels::goto::parse_count_expr;
use hxy_panels::goto::parse_offset_expr;
use hxy_panels::goto::parse_range_expr;
use palette_core::Entry;

/// The palette's cascade mode. `Main` is the root command list; the
/// rest are single-argument prompt modes reached from it. [`parent`]
/// drives the Escape-pops-back-one-level behaviour.
///
/// [`parent`]: PaletteMode::parent
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteMode {
    Main,
    GoToOffset,
    /// Virtual-address variant of [`Self::GoToOffset`]. The gpui port
    /// has no virtual-base plumbing yet, so no Main entry constructs it;
    /// the variant exists for cascade parity with the egui app and
    /// parses identically to `GoToOffset` if a future entry surfaces it.
    #[allow(dead_code)]
    GoToAddress,
    SelectFromOffset,
    SelectRange,
    SetColumns,
}

impl PaletteMode {
    /// One level up the cascade, or `None` at the root. Mirrors the
    /// egui app's `Mode::parent`: every argument mode collapses to
    /// `Main`, and `Main` itself closes the palette outright.
    pub fn parent(self) -> Option<Self> {
        match self {
            PaletteMode::Main => None,
            PaletteMode::GoToOffset
            | PaletteMode::GoToAddress
            | PaletteMode::SelectFromOffset
            | PaletteMode::SelectRange
            | PaletteMode::SetColumns => Some(PaletteMode::Main),
        }
    }

    /// Whether this mode treats the query as a raw argument rather than
    /// a fuzzy filter. Argument modes (and the `@` / `=` Main prefixes)
    /// bypass filtering so their single dynamic row is never hidden by
    /// a non-subsequence match against its human-readable label.
    pub fn bypasses_filter(self, query: &str) -> bool {
        match self {
            PaletteMode::Main => {
                let q = query.trim_start();
                q.starts_with('@') || q.starts_with('=')
            }
            _ => true,
        }
    }

    /// The i18n key for this mode's input placeholder / hint.
    pub fn hint_key(self) -> &'static str {
        match self {
            PaletteMode::Main => "palette-hint-main",
            PaletteMode::GoToOffset => "palette-hint-go-to-offset",
            PaletteMode::GoToAddress => "palette-hint-go-to-address",
            PaletteMode::SelectFromOffset => "palette-hint-select-from-offset",
            PaletteMode::SelectRange => "palette-hint-select-range",
            PaletteMode::SetColumns => "palette-hint-set-columns-local",
        }
    }
}

/// Which byte-format a [`PaletteAction::CopySelection`] writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyFormat {
    /// Space-separated uppercase hex, matching the vim hex-pane yank.
    Hex,
    /// Raw bytes as lossy UTF-8 text, matching the vim ASCII-pane yank.
    Bytes,
}

/// The pure-data outcome of activating an entry. The overlay handles
/// [`Self::SwitchMode`] / [`Self::NoOp`] itself; every other variant is
/// dispatched into the [`Workspace`](crate::workspace::Workspace).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaletteAction {
    OpenFile,
    CloseTab,
    ToggleVim,
    ToggleInspector,
    /// Cascade into an argument mode without closing the palette.
    SwitchMode(PaletteMode),
    /// Move the caret to an absolute offset (relative inputs are
    /// resolved against the cursor before the action is built).
    GoToOffset(u64),
    SetSelection { start: u64, end_exclusive: u64 },
    SetColumns(ColumnCount),
    /// Copy a literal string (the `=<expr>` calculator rows).
    CopyText(String),
    /// Copy the active file's current selection in the given format.
    CopySelection(CopyFormat),
    /// Inert: placeholder / invalid rows pick to this so a stray Enter
    /// doesn't get the user stuck; the overlay just closes.
    NoOp,
}

/// Snapshot of the active file the builders read to gate and resolve
/// entries. All-false / zero when no file is focused.
#[derive(Clone, Copy, Debug, Default)]
pub struct PaletteContext {
    pub has_active_file: bool,
    pub cursor: u64,
    pub source_len: u64,
    /// `Some((start, end_exclusive))` when a selection exists.
    pub selection: Option<(u64, u64)>,
    pub vim_on: bool,
}

/// Resolved keybinding hints for the Main-list commands that mirror a
/// workspace shortcut. `None` when no binding is registered.
#[derive(Clone, Debug, Default)]
pub struct Shortcuts {
    pub open_file: Option<String>,
    pub toggle_vim: Option<String>,
    pub toggle_inspector: Option<String>,
}

/// Ceiling for the palette's column-count input, matching the egui
/// app's slider cap: `ColumnCount` allows up to `u16::MAX`, but wider
/// than this is unreadable at sane font sizes.
const MAX_COLUMNS: u64 = 64;

/// Build the rows for `mode` given the current `query`, active-file
/// `ctx`, and resolved `shortcuts`.
pub fn build_entries(
    mode: PaletteMode,
    query: &str,
    ctx: PaletteContext,
    shortcuts: &Shortcuts,
) -> Vec<Entry<PaletteAction>> {
    let mut out = Vec::new();
    match mode {
        PaletteMode::Main => build_main_entries(&mut out, query, ctx, shortcuts),
        PaletteMode::GoToOffset
        | PaletteMode::GoToAddress
        | PaletteMode::SelectFromOffset
        | PaletteMode::SelectRange
        | PaletteMode::SetColumns => build_arg_entries(&mut out, mode, query.trim(), ctx),
    }
    out
}

fn build_main_entries(out: &mut Vec<Entry<PaletteAction>>, query: &str, ctx: PaletteContext, shortcuts: &Shortcuts) {
    // `@<expr>` jumps to a calculated offset; `=<expr>` copies a
    // calculated value. Either prefix replaces the whole Main list:
    // the user committed to the expression flow, and a fuzzy grab-bag
    // of unrelated commands underneath would be noise.
    let trimmed = query.trim_start();
    if let Some(rest) = trimmed.strip_prefix('@') {
        build_calculator_goto(out, rest, ctx);
        return;
    }
    if let Some(rest) = trimmed.strip_prefix('=') {
        build_calculator_copy(out, rest);
        return;
    }

    let mut open = Entry::new(hxy_i18n::t("toolbar-open-file"), PaletteAction::OpenFile);
    if let Some(hint) = &shortcuts.open_file {
        open = open.with_shortcut(hint.clone());
    }
    out.push(open);

    out.push(Entry::new(hxy_i18n::t("gpui-palette-close-tab"), PaletteAction::CloseTab).with_disabled(!ctx.has_active_file));

    let mut toggle_vim = Entry::new(hxy_i18n::t("palette-toggle-vim"), PaletteAction::ToggleVim).with_subtitle(
        hxy_i18n::t(if ctx.vim_on { "palette-toggle-vim-subtitle-on" } else { "palette-toggle-vim-subtitle-off" }),
    );
    if let Some(hint) = &shortcuts.toggle_vim {
        toggle_vim = toggle_vim.with_shortcut(hint.clone());
    }
    out.push(toggle_vim);

    let mut toggle_inspector =
        Entry::new(hxy_i18n::t("gpui-palette-toggle-inspector"), PaletteAction::ToggleInspector);
    if let Some(hint) = &shortcuts.toggle_inspector {
        toggle_inspector = toggle_inspector.with_shortcut(hint.clone());
    }
    out.push(toggle_inspector);

    out.push(
        Entry::new(hxy_i18n::t("palette-go-to-offset-entry"), PaletteAction::SwitchMode(PaletteMode::GoToOffset))
            .with_disabled(!ctx.has_active_file),
    );
    out.push(
        Entry::new(
            hxy_i18n::t("palette-select-from-offset-entry"),
            PaletteAction::SwitchMode(PaletteMode::SelectFromOffset),
        )
        .with_disabled(!ctx.has_active_file),
    );
    out.push(
        Entry::new(hxy_i18n::t("palette-select-range-entry"), PaletteAction::SwitchMode(PaletteMode::SelectRange))
            .with_disabled(!ctx.has_active_file),
    );
    out.push(
        Entry::new(hxy_i18n::t("palette-set-columns-local-entry"), PaletteAction::SwitchMode(PaletteMode::SetColumns))
            .with_disabled(!ctx.has_active_file),
    );

    let has_selection = ctx.selection.is_some();
    for (key, format) in [
        ("gpui-palette-copy-selection-hex", CopyFormat::Hex),
        ("gpui-palette-copy-selection-bytes", CopyFormat::Bytes),
    ] {
        let mut entry = Entry::new(hxy_i18n::t(key), PaletteAction::CopySelection(format)).with_disabled(!has_selection);
        if !has_selection {
            entry = entry.with_subtitle(hxy_i18n::t("gpui-palette-copy-selection-none"));
        }
        out.push(entry);
    }
}

/// Resolve a `@<expr>` query into a single Go-to-offset entry. Empty
/// expression renders an inert prompt; parse / evaluation / bounds
/// failures render one disabled "Invalid: ..." row.
fn build_calculator_goto(out: &mut Vec<Entry<PaletteAction>>, expr: &str, ctx: PaletteContext) {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        out.push(Entry::new(hxy_i18n::t("palette-go-to-offset-prompt"), PaletteAction::NoOp));
        return;
    }
    if !ctx.has_active_file {
        push_invalid(out, trimmed, &hxy_i18n::t("palette-invalid-no-active-file"));
        return;
    }
    let value = match hxy_calculator::evaluate_str_with(trimmed, &NullResolver) {
        Ok(v) => v,
        Err(e) => return push_invalid(out, trimmed, &e.to_string()),
    };
    let max_offset = ctx.source_len.saturating_sub(1);
    let target = match value.as_u64() {
        Ok(t) if t <= max_offset => t,
        Ok(t) => {
            let reason = hxy_i18n::t_args(
                "palette-calculator-out-of-range",
                &[("value", &format!("0x{t:X}")), ("max", &format!("0x{max_offset:X}"))],
            );
            return push_invalid(out, trimmed, &reason);
        }
        Err(e) => return push_invalid(out, trimmed, &e.to_string()),
    };
    out.push(
        Entry::new(
            hxy_i18n::t_args("palette-go-to-offset-fmt", &[("offset", &format!("0x{target:X}"))]),
            PaletteAction::GoToOffset(target),
        )
        .with_subtitle(format!("{trimmed} = {}", value.raw())),
    );
}

/// Resolve a `=<expr>` query into decimal + hex "Copy result" rows.
fn build_calculator_copy(out: &mut Vec<Entry<PaletteAction>>, expr: &str) {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        out.push(Entry::new(hxy_i18n::t("palette-copy-result-prompt"), PaletteAction::NoOp));
        return;
    }
    let value = match hxy_calculator::evaluate_str_with(trimmed, &NullResolver) {
        Ok(v) => v,
        Err(e) => return push_invalid(out, trimmed, &e.to_string()),
    };
    let raw = value.raw();
    let decimal = raw.to_string();
    let hex = format_signed_hex(raw);
    out.push(
        Entry::new(
            hxy_i18n::t_args("palette-copy-decimal-fmt", &[("value", &decimal)]),
            PaletteAction::CopyText(decimal.clone()),
        )
        .with_subtitle(hex.clone()),
    );
    out.push(
        Entry::new(hxy_i18n::t_args("palette-copy-hex-fmt", &[("value", &hex)]), PaletteAction::CopyText(hex))
            .with_subtitle(decimal),
    );
}

fn build_arg_entries(out: &mut Vec<Entry<PaletteAction>>, mode: PaletteMode, query: &str, ctx: PaletteContext) {
    if query.is_empty() {
        return;
    }
    if !ctx.has_active_file {
        push_invalid(out, query, &hxy_i18n::t("palette-invalid-no-active-file"));
        return;
    }
    match mode {
        PaletteMode::GoToOffset | PaletteMode::GoToAddress => {
            match parse_offset_expr(query, &NullResolver)
                .and_then(|n| n.resolve(ctx.cursor, ctx.source_len).ok_or(ParseError::OutOfRange))
            {
                Ok(target) => out.push(
                    Entry::new(
                        hxy_i18n::t_args("palette-go-to-offset-fmt", &[("offset", &format!("0x{target:X}"))]),
                        PaletteAction::GoToOffset(target),
                    )
                    .with_subtitle(format!("{target}")),
                ),
                Err(e) => push_invalid(out, query, &e.to_string()),
            }
        }
        PaletteMode::SelectFromOffset => match parse_count_expr(query, &NullResolver) {
            Ok(0) => push_invalid(out, query, &hxy_i18n::t("gpui-palette-invalid-nonzero")),
            Ok(count) => {
                let start = ctx.cursor;
                let available = ctx.source_len.saturating_sub(start);
                if available == 0 {
                    return push_invalid(out, query, &hxy_i18n::t("gpui-palette-invalid-at-eof"));
                }
                let end_exclusive = start + count.min(available);
                out.push(
                    Entry::new(
                        hxy_i18n::t_args(
                            "palette-select-from-offset-fmt",
                            &[("count", &(end_exclusive - start).to_string()), ("start", &format!("0x{start:X}"))],
                        ),
                        PaletteAction::SetSelection { start, end_exclusive },
                    )
                    .with_subtitle(format!("0x{start:X} .. 0x{end_exclusive:X}")),
                );
            }
            Err(e) => push_invalid(out, query, &e.to_string()),
        },
        PaletteMode::SelectRange => match parse_range_expr(query, ctx.source_len, &NullResolver) {
            Ok(range) => out.push(Entry::new(
                hxy_i18n::t_args(
                    "palette-select-range-fmt",
                    &[
                        ("start", &format!("0x{:X}", range.start)),
                        ("end", &format!("0x{:X}", range.end_exclusive)),
                        ("count", &range.len().to_string()),
                    ],
                ),
                PaletteAction::SetSelection { start: range.start, end_exclusive: range.end_exclusive },
            )),
            Err(e) => push_invalid(out, query, &e.to_string()),
        },
        PaletteMode::SetColumns => match parse_count_expr(query, &NullResolver) {
            Ok(n) if (1..=MAX_COLUMNS).contains(&n) => {
                // `n <= 64` fits `u16`, so `new` only fails on zero,
                // already excluded by the range check.
                match ColumnCount::new(n as u16) {
                    Ok(count) => out.push(Entry::new(
                        hxy_i18n::t_args("palette-set-columns-local-fmt", &[("count", &n.to_string())]),
                        PaletteAction::SetColumns(count),
                    )),
                    Err(e) => push_invalid(out, query, &e.to_string()),
                }
            }
            Ok(_) => push_invalid(
                out,
                query,
                &hxy_i18n::t_args("palette-invalid-columns-range", &[("max", &MAX_COLUMNS.to_string())]),
            ),
            Err(e) => push_invalid(out, query, &e.to_string()),
        },
        PaletteMode::Main => {}
    }
}

/// Push a disabled "Invalid: {reason}" row bound to [`PaletteAction::NoOp`].
fn push_invalid(out: &mut Vec<Entry<PaletteAction>>, query: &str, reason: &str) {
    out.push(
        Entry::new(hxy_i18n::t_args("palette-invalid-fmt", &[("reason", reason)]), PaletteAction::NoOp)
            .with_subtitle(query.to_owned())
            .with_disabled(true),
    );
}

/// Format a signed `i128` as a `0x...` literal, signed-magnitude:
/// `-16` renders `-0x10`, not a two's-complement pattern. Mirrors the
/// egui app's `format_signed_hex` so paste targets read the same.
fn format_signed_hex(value: i128) -> String {
    if value < 0 { format!("-0x{:X}", value.unsigned_abs()) } else { format!("0x{value:X}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active_ctx() -> PaletteContext {
        PaletteContext { has_active_file: true, cursor: 0, source_len: 256, selection: None, vim_on: false }
    }

    fn actions(entries: &[Entry<PaletteAction>]) -> Vec<PaletteAction> {
        entries.iter().map(|e| e.data.clone()).collect()
    }

    #[test]
    fn parent_cascade_collapses_to_main_then_none() {
        assert_eq!(PaletteMode::GoToOffset.parent(), Some(PaletteMode::Main));
        assert_eq!(PaletteMode::SelectRange.parent(), Some(PaletteMode::Main));
        assert_eq!(PaletteMode::SetColumns.parent(), Some(PaletteMode::Main));
        assert_eq!(PaletteMode::Main.parent(), None);
    }

    #[test]
    fn main_list_has_the_core_commands_and_gates_on_active_file() {
        let entries = build_entries(PaletteMode::Main, "", active_ctx(), &Shortcuts::default());
        let data = actions(&entries);
        assert!(data.contains(&PaletteAction::OpenFile));
        assert!(data.contains(&PaletteAction::CloseTab));
        assert!(data.contains(&PaletteAction::ToggleVim));
        assert!(data.contains(&PaletteAction::ToggleInspector));
        assert!(data.contains(&PaletteAction::SwitchMode(PaletteMode::GoToOffset)));
        assert!(data.contains(&PaletteAction::SwitchMode(PaletteMode::SetColumns)));
        // Every row enabled when a file is active (copy needs a
        // selection, so those two are the exception).
        let disabled: Vec<_> = entries.iter().filter(|e| e.disabled).map(|e| e.data.clone()).collect();
        assert_eq!(disabled, vec![
            PaletteAction::CopySelection(CopyFormat::Hex),
            PaletteAction::CopySelection(CopyFormat::Bytes),
        ]);
    }

    #[test]
    fn main_list_disables_file_commands_with_no_active_file() {
        let ctx = PaletteContext::default();
        let entries = build_entries(PaletteMode::Main, "", ctx, &Shortcuts::default());
        let find = |a: &PaletteAction| entries.iter().find(|e| &e.data == a).expect("row present");
        assert!(find(&PaletteAction::CloseTab).disabled);
        assert!(find(&PaletteAction::SwitchMode(PaletteMode::GoToOffset)).disabled);
        assert!(find(&PaletteAction::CopySelection(CopyFormat::Hex)).disabled);
        // Open File / Toggle Vim / Toggle Inspector stay enabled.
        assert!(!find(&PaletteAction::OpenFile).disabled);
        assert!(!find(&PaletteAction::ToggleVim).disabled);
    }

    #[test]
    fn copy_entries_enabled_when_selection_present() {
        let mut ctx = active_ctx();
        ctx.selection = Some((4, 8));
        let entries = build_entries(PaletteMode::Main, "", ctx, &Shortcuts::default());
        let hex = entries.iter().find(|e| e.data == PaletteAction::CopySelection(CopyFormat::Hex)).unwrap();
        assert!(!hex.disabled);
    }

    #[test]
    fn shortcut_hints_ride_the_entries_when_supplied() {
        let shortcuts = Shortcuts { open_file: Some("cmd-o".into()), ..Shortcuts::default() };
        let entries = build_entries(PaletteMode::Main, "", active_ctx(), &shortcuts);
        let open = entries.iter().find(|e| e.data == PaletteAction::OpenFile).unwrap();
        assert_eq!(open.shortcut.as_deref(), Some("cmd-o"));
    }

    #[test]
    fn go_to_offset_relative_resolves_against_cursor() {
        let mut ctx = active_ctx();
        ctx.cursor = 0x10;
        let entries = build_entries(PaletteMode::GoToOffset, "+10", ctx, &Shortcuts::default());
        assert_eq!(actions(&entries), vec![PaletteAction::GoToOffset(0x1A)]);
        assert!(!entries[0].disabled);
    }

    #[test]
    fn go_to_offset_absolute_and_hex() {
        let entries = build_entries(PaletteMode::GoToOffset, "0x20", active_ctx(), &Shortcuts::default());
        assert_eq!(actions(&entries), vec![PaletteAction::GoToOffset(0x20)]);
    }

    #[test]
    fn go_to_offset_invalid_renders_one_disabled_row() {
        let entries = build_entries(PaletteMode::GoToOffset, "not-a-number", active_ctx(), &Shortcuts::default());
        assert_eq!(entries.len(), 1);
        assert!(entries[0].disabled);
        assert_eq!(entries[0].data, PaletteAction::NoOp);
    }

    #[test]
    fn select_from_offset_clamps_to_available_bytes() {
        let mut ctx = active_ctx();
        ctx.cursor = 250;
        ctx.source_len = 256;
        let entries = build_entries(PaletteMode::SelectFromOffset, "100", ctx, &Shortcuts::default());
        assert_eq!(actions(&entries), vec![PaletteAction::SetSelection { start: 250, end_exclusive: 256 }]);
    }

    #[test]
    fn select_range_parses_start_end() {
        let entries = build_entries(PaletteMode::SelectRange, "0x10..0x20", active_ctx(), &Shortcuts::default());
        assert_eq!(actions(&entries), vec![PaletteAction::SetSelection { start: 0x10, end_exclusive: 0x20 }]);
    }

    #[test]
    fn set_columns_valid_and_out_of_range() {
        let ok = build_entries(PaletteMode::SetColumns, "24", active_ctx(), &Shortcuts::default());
        assert_eq!(actions(&ok), vec![PaletteAction::SetColumns(ColumnCount::new(24).unwrap())]);
        let bad = build_entries(PaletteMode::SetColumns, "999", active_ctx(), &Shortcuts::default());
        assert_eq!(bad.len(), 1);
        assert!(bad[0].disabled);
    }

    #[test]
    fn calculator_at_prefix_builds_a_goto_row() {
        let entries = build_entries(PaletteMode::Main, "@0x10 + 0x10", active_ctx(), &Shortcuts::default());
        assert_eq!(actions(&entries), vec![PaletteAction::GoToOffset(0x20)]);
    }

    #[test]
    fn calculator_at_out_of_range_is_disabled() {
        let mut ctx = active_ctx();
        ctx.source_len = 16;
        let entries = build_entries(PaletteMode::Main, "@0x1000", ctx, &Shortcuts::default());
        assert_eq!(entries.len(), 1);
        assert!(entries[0].disabled);
    }

    #[test]
    fn calculator_equals_builds_decimal_and_hex_copy_rows() {
        let entries = build_entries(PaletteMode::Main, "=2+2", active_ctx(), &Shortcuts::default());
        assert_eq!(actions(&entries), vec![
            PaletteAction::CopyText("4".into()),
            PaletteAction::CopyText("0x4".into()),
        ]);
    }

    #[test]
    fn bypass_filter_engages_for_arg_modes_and_calc_prefixes() {
        assert!(PaletteMode::GoToOffset.bypasses_filter(""));
        assert!(PaletteMode::Main.bypasses_filter("@0x10"));
        assert!(PaletteMode::Main.bypasses_filter("=2+2"));
        assert!(!PaletteMode::Main.bypasses_filter("open"));
    }
}
