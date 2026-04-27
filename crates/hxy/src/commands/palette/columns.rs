//! Column-count entry builder for the palette's
//! `Set columns (this buffer / default)` modes.

use crate::commands::palette::Action;
use crate::commands::palette::ColumnScope;
use crate::commands::palette::Mode;

/// Match the Settings panel's slider cap so a user can't end up
/// with a hex view they can't comfortably read. The underlying
/// [`hxy_core::ColumnCount`] allows up to `u16::MAX`, but anything
/// above this overflows even ultrawide monitors at sane font sizes.
pub const PALETTE_MAX_COLUMNS: u16 = 64;

pub fn build_columns_entries(
    out: &mut Vec<egui_palette::Entry<Action>>,
    mode: Mode,
    query: &str,
) {
    use egui_phosphor::regular as icon;

    if query.is_empty() {
        return;
    }
    let scope = match mode {
        Mode::SetColumnsLocal => ColumnScope::Local,
        Mode::SetColumnsGlobal => ColumnScope::Global,
        _ => return,
    };
    let parsed = match crate::commands::goto::parse_number(query) {
        Ok(crate::commands::goto::Number::Absolute(n)) => n,
        Ok(crate::commands::goto::Number::Relative(_)) => {
            super::entries::invalid_entry(out, query, "column count must be absolute (no + / - prefix)");
            return;
        }
        Err(e) => {
            super::entries::invalid_entry(out, query, &e.to_string());
            return;
        }
    };
    let n_u16 = match u16::try_from(parsed) {
        Ok(n) if (1..=u64::from(PALETTE_MAX_COLUMNS)).contains(&parsed) => n,
        _ => {
            super::entries::invalid_entry(
                out,
                query,
                &hxy_i18n::t_args("palette-invalid-columns-range", &[("max", &PALETTE_MAX_COLUMNS.to_string())]),
            );
            return;
        }
    };
    let count = match hxy_core::ColumnCount::new(n_u16) {
        Ok(c) => c,
        Err(e) => {
            super::entries::invalid_entry(out, query, &e.to_string());
            return;
        }
    };
    let (key, scope_icon) = match scope {
        ColumnScope::Local => ("palette-set-columns-local-fmt", icon::COLUMNS),
        ColumnScope::Global => ("palette-set-columns-global-fmt", icon::COLUMNS_PLUS_RIGHT),
    };
    out.push(
        egui_palette::Entry::new(
            hxy_i18n::t_args(key, &[("count", &n_u16.to_string())]),
            Action::SetColumns { scope, count },
        )
        .with_icon(scope_icon),
    );
}
