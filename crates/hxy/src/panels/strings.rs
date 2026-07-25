//! Strings tool.
//!
//! Extraction (encoding, config, scanner) moved to
//! `hxy_panels::strings` (framework-agnostic, shared with the GPUI
//! port); re-exported here under the original path. Worker
//! dispatch and rendering stay here -- they touch `FileId`,
//! `egui_inbox`, and the background pool.

use std::sync::Arc;

use hxy_core::ByteRange;
use hxy_core::HexSource;
pub use hxy_panels::strings::DEFAULT_MIN_LENGTH;
pub use hxy_panels::strings::Encoding;
pub use hxy_panels::strings::MAX_RESULTS;
pub use hxy_panels::strings::SortColumn;
pub use hxy_panels::strings::SortOrder;
pub use hxy_panels::strings::StringEntry;
pub use hxy_panels::strings::StringsConfig;
pub use hxy_panels::strings::StringsEvent;
pub use hxy_panels::strings::StringsResult;
pub use hxy_panels::strings::extract;
pub use hxy_panels::strings::sort_entries;

use crate::files::FileId;

#[derive(Clone, Debug)]
pub enum StringsOutcome {
    Ok(StringsResult),
    Err(String),
}

pub struct StringsComputation {
    pub inbox: egui_inbox::UiInbox<StringsOutcome>,
    pub file_id: FileId,
    pub started: web_time::Instant,
}

#[derive(Default)]
pub struct StringsPanel {
    pub config: StringsConfig,
    pub last_result: Option<StringsResult>,
    pub running: Option<StringsComputation>,
    /// Substring filter applied client-side over the result list.
    /// Held on the panel rather than recomputed each frame so it
    /// survives view scroll / repaint.
    pub filter: String,
    /// Active sort applied to `last_result.entries`. Defaults to
    /// ascending offset, matching the order the extractor produces.
    pub sort: SortOrder,
    /// Byte range the pointer is currently over in the result table,
    /// used by the hex view to paint a hover highlight on the
    /// matched bytes. Mirrors `TemplateState::hovered_node` -- the
    /// hex view reads from here whenever the pointer rests over a
    /// row in this panel. Reset to `None` each frame when no cell
    /// sees the pointer; also cleared on tab close so a stale value
    /// doesn't keep the highlight stuck after the panel goes away.
    pub hovered_entry: Option<ByteRange>,
}

/// Spin up a strings worker. Returns the in-flight handle the host
/// stashes on `OpenFile::strings_panel.running`.
pub fn spawn_compute(
    ctx: &egui::Context,
    id: FileId,
    source: Arc<dyn HexSource>,
    config: StringsConfig,
) -> StringsComputation {
    let (sender, inbox) = egui_inbox::UiInbox::channel_with_ctx(ctx);
    let started = web_time::Instant::now();
    crate::background::submit(move || {
        let outcome = match extract(&*source, &config) {
            Ok(result) => StringsOutcome::Ok(result),
            Err(e) => StringsOutcome::Err(e),
        };
        let _ = sender.send(outcome);
    });
    StringsComputation { inbox, file_id: id, started }
}

/// Render the per-file Strings panel without virtual addressing
/// (offset / end columns show raw file offsets). Returns user-
/// emitted events so the host can dispatch them (run / jump)
/// without taking a `&mut HxyApp` borrow during rendering.
pub fn show(ui: &mut egui::Ui, file_label: Option<&str>, panel: &mut StringsPanel) -> Vec<StringsEvent> {
    show_inner(ui, file_label, panel, None)
}

/// Render the per-file Strings panel with virtual addressing
/// applied: offset / end values are rendered as `entry + base`
/// and the column headers switch to "Address" / "End address".
/// Use this variant only when the file has an accepted virtual
/// base; otherwise call [`show`].
pub fn show_with_vaddr(
    ui: &mut egui::Ui,
    file_label: Option<&str>,
    panel: &mut StringsPanel,
    virtual_base: u64,
) -> Vec<StringsEvent> {
    show_inner(ui, file_label, panel, Some(virtual_base))
}

fn show_inner(
    ui: &mut egui::Ui,
    file_label: Option<&str>,
    panel: &mut StringsPanel,
    virtual_base: Option<u64>,
) -> Vec<StringsEvent> {
    let mut events: Vec<StringsEvent> = Vec::new();
    ui.horizontal(|ui| {
        ui.heading(hxy_i18n::t("strings-heading"));
        ui.add_space(8.0);
        let label = file_label.unwrap_or("");
        ui.label(egui::RichText::new(label).weak());
    });
    ui.separator();

    if file_label.is_none() {
        ui.label(hxy_i18n::t("strings-no-active-file"));
        return events;
    }

    let running = panel.running.is_some();

    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("strings-encoding").selected_text(panel.config.encoding.label()).show_ui(
            ui,
            |ui| {
                for enc in Encoding::ALL {
                    ui.selectable_value(&mut panel.config.encoding, enc, enc.label());
                }
            },
        );
        ui.label(hxy_i18n::t("strings-min-length"));
        let mut min: u64 = panel.config.min_length as u64;
        ui.add(egui::DragValue::new(&mut min).range(1..=4096));
        panel.config.min_length = min.max(1) as usize;

        let run_label = if running { hxy_i18n::t("strings-running") } else { hxy_i18n::t("strings-run") };
        let run_button = egui::Button::new(run_label);
        if ui.add_enabled(!running, run_button).clicked() {
            events.push(StringsEvent::Run);
        }
    });

    let range = panel.config.range;
    if !range.is_empty() {
        let base = virtual_base.unwrap_or(0);
        ui.label(hxy_i18n::t_args(
            "strings-range",
            &[
                ("start", &format!("0x{:X}", range.start().get().saturating_add(base))),
                ("end", &format!("0x{:X}", range.end().get().saturating_add(base))),
                ("length", &format_bytes(range.len().get())),
            ],
        ));
    }

    ui.horizontal(|ui| {
        ui.label(hxy_i18n::t("strings-filter"));
        ui.add(egui::TextEdit::singleline(&mut panel.filter).desired_width(180.0));
    });

    ui.separator();

    let Some(result) = panel.last_result.as_ref() else {
        if running {
            ui.label(hxy_i18n::t("strings-running"));
        } else {
            ui.label(hxy_i18n::t("strings-no-results-yet"));
        }
        return events;
    };

    let filter = panel.filter.trim().to_lowercase();
    let total = result.entries.len();

    let summary = if filter.is_empty() {
        hxy_i18n::t_args("strings-summary", &[("count", &total.to_string())])
    } else {
        hxy_i18n::t_args("strings-summary-filtered", &[("count", &total.to_string()), ("filter", &panel.filter)])
    };
    ui.label(summary);
    if result.truncated {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            hxy_i18n::t_args("strings-truncated", &[("max", &MAX_RESULTS.to_string())]),
        );
    }

    // Re-sort the entries vector in place when the panel's sort
    // order has drifted from what the result was last sorted by.
    // Stored on the result rather than recomputed each frame so a
    // re-render with the same sort doesn't pay the sort cost.
    if result.sorted_by != panel.sort {
        let order = panel.sort;
        let result_mut = panel.last_result.as_mut().expect("matched as Some above");
        sort_entries(&mut result_mut.entries, order);
        result_mut.sorted_by = order;
    }
    let result = panel.last_result.as_ref().expect("matched as Some above");

    // Filter is applied as a Vec<usize> of indices into the
    // (possibly re-sorted) entries vector so the egui_table delegate
    // can map row_nr -> entry without rescanning each frame.
    let visible: Vec<usize> = if filter.is_empty() {
        (0..result.entries.len()).collect()
    } else {
        result
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| e.text.to_lowercase().contains(&filter).then_some(i))
            .collect()
    };

    if visible.is_empty() {
        if !filter.is_empty() {
            ui.label(hxy_i18n::t("strings-no-matches"));
        }
        return events;
    }

    let mut delegate = StringsTableDelegate {
        entries: &result.entries,
        visible: &visible,
        sort: panel.sort,
        pending_sort: None,
        pending_hover: None,
        virtual_base,
        events: &mut events,
    };

    let row_height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
    let table = egui_table::Table::new()
        .id_salt("hxy-strings-table")
        .num_rows(visible.len() as u64)
        .columns(vec![
            egui_table::Column::new(110.0).range(70.0..=200.0).resizable(true).id(egui::Id::new("strings-col-offset")),
            egui_table::Column::new(110.0).range(70.0..=200.0).resizable(true).id(egui::Id::new("strings-col-end")),
            egui_table::Column::new(80.0).range(50.0..=160.0).resizable(true).id(egui::Id::new("strings-col-length")),
            egui_table::Column::new(360.0).range(80.0..=2000.0).resizable(true).id(egui::Id::new("strings-col-text")),
        ])
        .headers(vec![egui_table::HeaderRow::new(row_height + 2.0)])
        .auto_size_mode(egui_table::AutoSizeMode::Always);
    table.show(ui, &mut delegate);

    // Copy values out of the delegate before any further mutable
    // borrow of `panel`, since the delegate still holds a shared
    // borrow on `panel.last_result.entries` until it goes out of
    // scope. ByteRange and SortOrder are Copy.
    let new_sort = delegate.pending_sort;
    let new_hover = delegate.pending_hover;
    if let Some(s) = new_sort {
        panel.sort = s;
    }
    panel.hovered_entry = new_hover;

    events
}

struct StringsTableDelegate<'a> {
    entries: &'a [StringEntry],
    visible: &'a [usize],
    sort: SortOrder,
    /// Set by `header_cell_ui` when the user clicked a column
    /// header. The caller writes it back onto the panel after
    /// `Table::show` returns.
    pending_sort: Option<SortOrder>,
    /// Byte range of the row whose cell currently contains the
    /// pointer, or `None` when no cell sees the pointer this frame.
    /// Mirrored back onto `panel.hovered_entry` post-render so the
    /// hex view picks it up.
    pending_hover: Option<ByteRange>,
    /// Active virtual base. When `Some`, offset / end columns
    /// render as virtual addresses and headers swap to "Address" /
    /// "End address" labels.
    virtual_base: Option<u64>,
    events: &'a mut Vec<StringsEvent>,
}

impl egui_table::TableDelegate for StringsTableDelegate<'_> {
    fn header_cell_ui(&mut self, ui: &mut egui::Ui, cell: &egui_table::HeaderCellInfo) {
        let (label_key, sort_col) = match cell.col_range.start {
            0 => {
                let key = if self.virtual_base.is_some() { "strings-col-address" } else { "strings-col-offset" };
                (key, SortColumn::Offset)
            }
            1 => {
                let key = if self.virtual_base.is_some() { "strings-col-end-address" } else { "strings-col-end" };
                (key, SortColumn::End)
            }
            2 => ("strings-col-length", SortColumn::Length),
            3 => ("strings-col-text", SortColumn::Text),
            _ => return,
        };
        let mut text = hxy_i18n::t(label_key);
        if self.sort.column() == sort_col {
            let glyph = if self.sort.is_descending() {
                egui_phosphor::regular::CARET_DOWN
            } else {
                egui_phosphor::regular::CARET_UP
            };
            text.push(' ');
            text.push_str(glyph);
        }
        ui.add_space(6.0);
        let resp = ui.add(egui::Label::new(egui::RichText::new(text).strong()).sense(egui::Sense::click()));
        if resp.clicked() {
            self.pending_sort = Some(self.sort.cycle(sort_col));
        }
    }

    fn cell_ui(&mut self, ui: &mut egui::Ui, cell: &egui_table::CellInfo) {
        let row = cell.row_nr as usize;
        let Some(entry_idx) = self.visible.get(row).copied() else { return };
        let Some(entry) = self.entries.get(entry_idx) else { return };
        // Pointer-over-cell counts as pointer-over-row for the hex
        // view hover highlight: every cell in a row resolves to the
        // same byte range, so the last cell that sees the pointer
        // wins each frame and produces a stable hover signal.
        if ui.rect_contains_pointer(ui.max_rect())
            && let Ok(range) = ByteRange::new(hxy_core::ByteOffset::new(entry.offset), hxy_core::ByteOffset::new(entry.end))
        {
            self.pending_hover = Some(range);
        }
        ui.add_space(4.0);
        let base = self.virtual_base.unwrap_or(0);
        match cell.col_nr {
            0 => {
                let display = entry.offset.saturating_add(base);
                if ui.link(egui::RichText::new(format!("0x{display:X}")).monospace()).clicked() {
                    self.events.push(StringsEvent::Jump { offset: entry.offset, end: entry.end });
                }
            }
            1 => {
                let display = entry.end.saturating_add(base);
                ui.monospace(format!("0x{display:X}"));
            }
            2 => {
                ui.monospace(format!("{}", entry.length()));
            }
            3 => {
                ui.add(
                    egui::Label::new(egui::RichText::new(&entry.text).monospace())
                        .wrap_mode(egui::TextWrapMode::Extend)
                        .selectable(true),
                );
            }
            _ => {}
        }
    }
}

fn format_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else if n < 1024 * 1024 * 1024 {
        format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GiB", n as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}
