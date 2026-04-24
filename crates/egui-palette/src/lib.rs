//! A command-palette widget for egui -- the Cmd+P / Ctrl+Shift+P
//! control familiar from VS Code, Zed, and Sublime.
//!
//! The crate owns the UI, keyboard navigation, and fuzzy-match
//! scoring; the caller owns what the entries *mean*. Each [`Entry`]
//! carries an arbitrary `data: A` payload; when the user activates
//! an entry, [`show`] returns that payload so the caller can route
//! it through its own action handler.
//!
//! ```ignore
//! let entries = vec![
//!     Entry::new("Open file", MyAction::Open),
//!     Entry::new("Close file", MyAction::Close).with_subtitle("Cmd+W"),
//! ];
//! if let Some(Outcome::Picked(action)) = egui_palette::show(ctx, &mut state, &entries, "Search...") {
//!     dispatch(action);
//! }
//! ```
//!
//! Customise the look via [`Style`] and [`show_with_style`]:
//!
//! ```ignore
//! let style = Style::default()
//!     .anchored_at(Anchor::Center)
//!     .width_range(320.0, 480.0);
//! egui_palette::show_with_style(ctx, &mut state, &entries, "Search...", &style);
//! ```
//!
//! Cascading / modes / keyboard-shortcut binding are all out of
//! scope -- the host decides when to call `show`, rebuilds the
//! entry list as state changes, and re-opens `state` on a new mode
//! by clearing it between frames.

#![forbid(unsafe_code)]

use std::borrow::Cow;

use egui::Color32;
use egui::Pos2;
use egui::Stroke;

pub mod fuzzy;

/// Re-exports so callers can configure the matcher without pulling
/// `nucleo_matcher` into their own `Cargo.toml`.
pub use nucleo_matcher::Config as MatcherConfig;
pub use nucleo_matcher::pattern::CaseMatching;
pub use nucleo_matcher::pattern::Normalization;

/// Persistent palette state held by the host between frames.
/// Cleared / re-opened explicitly by the host (via [`State::open`]
/// / [`State::close`]); the widget itself mutates only `query`,
/// `selected`, and `pending_focus` during its lifetime.
#[derive(Default)]
pub struct State {
    pub open: bool,
    pub query: String,
    pub selected: usize,
    /// Set by [`State::open`]; consumed by the widget to
    /// `request_focus` on the text input on its first frame.
    pub pending_focus: bool,
    /// Snapshot of `query` from the previous frame. When the
    /// palette detects `query != last_query` it snaps `selected`
    /// back to the top (best match), matching VS Code / Zed UX.
    last_query: String,
}

impl State {
    /// Mark the palette as open and reset query / selection. Call
    /// this when you want a fresh search (e.g. on first open or
    /// when switching cascade modes).
    pub fn open(&mut self) {
        self.open = true;
        self.query.clear();
        self.last_query.clear();
        self.selected = 0;
        self.pending_focus = true;
    }

    pub fn close(&mut self) {
        self.open = false;
    }
}

/// One selectable row. `data` is returned verbatim in
/// [`Outcome::Picked`]; the crate doesn't care what it is.
pub struct Entry<A> {
    pub title: String,
    pub subtitle: Option<String>,
    /// Optional leading icon (single glyph / short string). Rendered
    /// in a fixed-width gutter on the left of the row.
    pub icon: Option<String>,
    pub data: A,
}

impl<A> Entry<A> {
    pub fn new(title: impl Into<String>, data: A) -> Self {
        Self { title: title.into(), subtitle: None, icon: None, data }
    }

    pub fn with_subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    pub fn with_icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }
}

/// What happened this frame. [`Outcome::Picked`] carries a *clone* of
/// the matching entry's `data`; [`Outcome::Closed`] fires on `Esc`
/// or a click on the backdrop.
pub enum Outcome<A> {
    Picked(A),
    Closed,
}

/// Where the panel sits inside the content rect.
#[derive(Clone, Copy, Debug)]
pub enum Anchor {
    /// Horizontally centred, `y_offset` points below the top edge of
    /// the content rect. Default (72 px) matches VS Code / Zed.
    TopCenter { y_offset: f32 },
    /// Centred on both axes.
    Center,
    /// Caller-supplied top-left position. Use when you want the
    /// palette anchored to a specific widget (an omnibar etc.).
    Manual(Pos2),
}

impl Default for Anchor {
    fn default() -> Self {
        Self::TopCenter { y_offset: 72.0 }
    }
}

/// Everything tweakable about the palette. All `Option<Color32>` /
/// `Option<Stroke>` fields use egui's theme visuals when `None`, so
/// the defaults track light-mode / dark-mode switches automatically.
#[derive(Clone)]
pub struct Style {
    // ---- Position ----
    pub anchor: Anchor,

    // ---- Dimensions (in egui points) ----
    pub min_width: f32,
    pub max_width: f32,
    /// Target width as a fraction of the content-rect width, clamped
    /// into `[min_width, max_width]`.
    pub width_fraction: f32,
    pub row_height: f32,
    pub icon_size: f32,
    /// Horizontal space between the icon's left edge and the title's
    /// left edge. Keep this wide enough for your largest glyph.
    pub icon_gutter: f32,
    /// Spacing between title and subtitle on the same row.
    pub subtitle_spacing: f32,
    pub inner_margin: egui::Margin,
    pub corner_radius: egui::CornerRadius,
    /// Hard ceiling for the result list height. Also the value used
    /// when the viewport is too short to derive one automatically.
    pub list_max_height: f32,
    pub list_min_height: f32,
    /// Subtracted from viewport height when sizing the scroll list
    /// (padding reserved for the text input + margins).
    pub row_reserve: f32,

    // ---- Colours (None = follow egui::Visuals) ----
    /// Full-viewport overlay painted behind the panel. `None` means
    /// no backdrop at all (the palette floats on top of unmodified
    /// app UI, e.g. for inline / always-open palettes).
    pub backdrop: Option<Color32>,
    /// Panel background fill. `None` uses [`egui::Frame::popup`]'s
    /// theme-derived colour.
    pub panel_fill: Option<Color32>,
    /// Panel outline. `None` leaves [`egui::Frame::popup`]'s default.
    pub panel_stroke: Option<Stroke>,
    /// Fill painted behind the currently-selected row. `None`
    /// derives from `visuals.selection.bg_fill` with 0.4 opacity so
    /// it reads on both light and dark themes.
    pub selected_fill: Option<Color32>,
    /// Colour of the entry title and the icon. `None` uses
    /// `visuals.text_color()`.
    pub text_color: Option<Color32>,
    /// Colour of the entry subtitle. `None` uses
    /// `visuals.weak_text_color()`.
    pub subtitle_color: Option<Color32>,
    /// Font size used for the subtitle. `None` falls back to the
    /// size of [`egui::TextStyle::Small`] (noticeably smaller than
    /// the title so a long path reads as secondary).
    pub subtitle_size: Option<f32>,
    /// Colour of the icon glyph. `None` uses [`Self::text_color`] so
    /// icons and titles match unless explicitly split.
    pub icon_color: Option<Color32>,

    // ---- Behaviour ----
    /// Close the palette when the backdrop is clicked. Default
    /// `true`; set `false` to make clicks outside the panel a no-op.
    pub close_on_backdrop_click: bool,
    /// Consume `ArrowUp` / `ArrowDown` / `Enter` events so they
    /// drive the palette instead of bubbling to the text input.
    /// Default `true`; turn off if you're composing with something
    /// that needs those keys.
    pub consume_nav_keys: bool,

    /// Scoring weights passed to [`nucleo_matcher::Matcher::new`].
    /// Defaults to [`MatcherConfig::DEFAULT`] (VS Code / Helix);
    /// swap in `MatcherConfig::DEFAULT.match_paths()` for path-style
    /// candidates, for instance.
    pub matcher: MatcherConfig,
    /// How the pattern's capitalisation should affect matching.
    /// Default [`CaseMatching::Smart`]: lowercase query ->
    /// case-insensitive, any uppercase -> case-sensitive.
    pub case_matching: CaseMatching,
    /// Unicode normalisation applied to both the pattern and each
    /// haystack. Default [`Normalization::Smart`].
    pub normalization: Normalization,

    /// Keys that dismiss the palette without picking an entry.
    /// Defaults to `[Escape]`. Set to `&[]` to disable keyboard
    /// dismissal entirely (backdrop click still works if
    /// [`Style::close_on_backdrop_click`] is on); add more keys to
    /// support alternative bindings like Ctrl+G.
    pub dismiss_keys: Cow<'static, [egui::Key]>,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            anchor: Anchor::default(),
            min_width: 360.0,
            max_width: 560.0,
            width_fraction: 0.38,
            row_height: 22.0,
            icon_size: 14.0,
            icon_gutter: 20.0,
            subtitle_spacing: 8.0,
            inner_margin: egui::Margin::symmetric(12, 10),
            corner_radius: egui::CornerRadius::same(8),
            list_max_height: 560.0,
            list_min_height: 200.0,
            row_reserve: 96.0,
            backdrop: Some(Color32::from_black_alpha(120)),
            panel_fill: None,
            panel_stroke: None,
            selected_fill: None,
            text_color: None,
            subtitle_color: None,
            subtitle_size: None,
            icon_color: None,
            close_on_backdrop_click: true,
            consume_nav_keys: true,
            matcher: MatcherConfig::DEFAULT,
            case_matching: CaseMatching::Smart,
            normalization: Normalization::Smart,
            dismiss_keys: Cow::Borrowed(&[egui::Key::Escape]),
        }
    }
}

impl Style {
    /// Position the panel via the given [`Anchor`].
    pub fn anchored_at(mut self, anchor: Anchor) -> Self {
        self.anchor = anchor;
        self
    }

    /// Set both width bounds in a single call.
    pub fn width_range(mut self, min: f32, max: f32) -> Self {
        self.min_width = min;
        self.max_width = max;
        self
    }

    /// Override the colour of the semi-transparent backdrop, or
    /// pass `None` to disable the backdrop entirely.
    pub fn backdrop_fill(mut self, fill: Option<Color32>) -> Self {
        self.backdrop = fill;
        self
    }

    /// Convenience: override panel fill + outline together.
    pub fn panel_colours(mut self, fill: Color32, stroke: Stroke) -> Self {
        self.panel_fill = Some(fill);
        self.panel_stroke = Some(stroke);
        self
    }
}

/// Render the palette modal and return an outcome if the user
/// activated an entry or dismissed the panel this frame. Returns
/// `None` on idle frames (still typing, still moving selection,
/// still rendering); the host should early-return when the palette
/// isn't visible by checking `state.open`.
pub fn show<A: Clone>(ctx: &egui::Context, state: &mut State, entries: &[Entry<A>], hint: &str) -> Option<Outcome<A>> {
    show_with_style(ctx, state, entries, hint, &Style::default())
}

/// Variant of [`show`] that takes an explicit [`Style`].
pub fn show_with_style<A: Clone>(
    ctx: &egui::Context,
    state: &mut State,
    entries: &[Entry<A>],
    hint: &str,
    style: &Style,
) -> Option<Outcome<A>> {
    if !state.open {
        return None;
    }

    // Drain matching key-press events so downstream handlers don't
    // also react to the same press (e.g. clearing a hex-editor
    // selection when the user hit Esc only to dismiss the palette).
    let dismissed = ctx.input_mut(|i| {
        let mut found = false;
        i.events.retain(|event| {
            let egui::Event::Key { key, pressed: true, repeat: false, .. } = event else {
                return true;
            };
            if style.dismiss_keys.iter().any(|k| k == key) {
                found = true;
                return false;
            }
            true
        });
        found
    });
    if dismissed {
        return Some(Outcome::Closed);
    }

    let filtered =
        fuzzy::filter_and_sort(&state.query, entries, &style.matcher, style.case_matching, style.normalization, |e| {
            match &e.subtitle {
                Some(sub) => std::borrow::Cow::Owned(format!("{} {}", e.title, sub)),
                None => std::borrow::Cow::Borrowed(e.title.as_str()),
            }
        });
    if state.query != state.last_query {
        // The top-scoring result almost always moved on a query
        // change, so drop the user back to row 0 instead of leaving
        // selection on whatever row a stale index happened to point
        // at.
        state.selected = 0;
        // Reuse the `last_query` buffer instead of allocating a
        // fresh String every time the query changes; the typical
        // edit tacks on or deletes a handful of bytes, so the
        // existing capacity will fit.
        state.last_query.clone_from(&state.query);
    } else if !filtered.is_empty() {
        state.selected = state.selected.min(filtered.len() - 1);
    } else {
        state.selected = 0;
    }

    let mut picked_idx: Option<usize> = None;
    if style.consume_nav_keys {
        ctx.input_mut(|i| {
            if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) && !filtered.is_empty() {
                state.selected = (state.selected + 1) % filtered.len();
            }
            if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) && !filtered.is_empty() {
                state.selected = (state.selected + filtered.len() - 1) % filtered.len();
            }
            if i.consume_key(egui::Modifiers::NONE, egui::Key::Enter) && !filtered.is_empty() {
                picked_idx = Some(state.selected);
            }
        });
    }

    let screen_rect = ctx.content_rect();

    if let Some(fill) = style.backdrop {
        let mut backdrop_click = false;
        egui::Area::new(egui::Id::new("egui_palette_backdrop"))
            .fixed_pos(screen_rect.min)
            .order(egui::Order::Middle)
            .interactable(true)
            .show(ctx, |ui| {
                let (rect, resp) = ui.allocate_exact_size(screen_rect.size(), egui::Sense::click());
                ui.painter().rect_filled(rect, 0.0, fill);
                if resp.clicked() {
                    backdrop_click = true;
                }
            });
        if backdrop_click && style.close_on_backdrop_click {
            return Some(Outcome::Closed);
        }
    }

    let panel_width = (screen_rect.width() * style.width_fraction).clamp(style.min_width, style.max_width);
    let (panel_x, panel_y) = match style.anchor {
        Anchor::TopCenter { y_offset } => (screen_rect.center().x - panel_width * 0.5, screen_rect.top() + y_offset),
        Anchor::Center => {
            (screen_rect.center().x - panel_width * 0.5, screen_rect.center().y - screen_rect.height() * 0.25)
        }
        Anchor::Manual(pos) => (pos.x, pos.y),
    };
    let list_max_height =
        (screen_rect.height() - panel_y - style.row_reserve).clamp(style.list_min_height, style.list_max_height);

    egui::Area::new(egui::Id::new("egui_palette"))
        .fixed_pos(egui::pos2(panel_x, panel_y))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            let frame = match (style.panel_fill, style.panel_stroke) {
                (Some(fill), Some(stroke)) => egui::Frame::new()
                    .fill(fill)
                    .stroke(stroke)
                    .inner_margin(style.inner_margin)
                    .corner_radius(style.corner_radius),
                (Some(fill), None) => egui::Frame::popup(ui.style())
                    .fill(fill)
                    .inner_margin(style.inner_margin)
                    .corner_radius(style.corner_radius),
                (None, Some(stroke)) => egui::Frame::popup(ui.style())
                    .stroke(stroke)
                    .inner_margin(style.inner_margin)
                    .corner_radius(style.corner_radius),
                (None, None) => {
                    egui::Frame::popup(ui.style()).inner_margin(style.inner_margin).corner_radius(style.corner_radius)
                }
            };
            frame.show(ui, |ui| {
                ui.set_min_width(panel_width);
                ui.set_max_width(panel_width);

                let text_edit =
                    egui::TextEdit::singleline(&mut state.query).hint_text(hint).desired_width(f32::INFINITY);
                let resp = ui.add(text_edit);
                // Keep focus glued to the query field the whole time
                // the palette is open, so arrow-key list navigation
                // doesn't steal typing focus and Left/Right still
                // drive the text cursor. `pending_focus` is the
                // first-frame trigger; after that we simply refuse
                // to let focus drift.
                if state.pending_focus {
                    resp.request_focus();
                    state.pending_focus = false;
                } else if !resp.has_focus() {
                    resp.request_focus();
                }

                ui.add_space(6.0);
                // Sync selection from hover only while the pointer
                // is actually moving. Without this gate, opening the
                // palette with the cursor already over the list area
                // would slam `selected` to whatever row it started on
                // -- often the bottom row the user was hovering when
                // they hit Cmd+P -- instead of the intended row 0.
                let pointer_moving = ui.ctx().input(|i| i.pointer.delta() != egui::Vec2::ZERO);
                egui::ScrollArea::vertical().max_height(list_max_height).auto_shrink([false, false]).show(ui, |ui| {
                    for (row, idx) in filtered.iter().enumerate() {
                        let entry = &entries[*idx];
                        let selected = row == state.selected;
                        let resp = render_row(ui, entry, selected, style);
                        if resp.clicked() {
                            picked_idx = Some(row);
                        }
                        if resp.hovered() && pointer_moving {
                            state.selected = row;
                        }
                    }
                    if filtered.is_empty() {
                        ui.add_space(16.0);
                        ui.vertical_centered(|ui| {
                            ui.weak("No matches.");
                        });
                        ui.add_space(16.0);
                    }
                });
            });
        });

    if let Some(row) = picked_idx
        && let Some(&idx) = filtered.get(row)
    {
        return Some(Outcome::Picked(entries[idx].data.clone()));
    }
    None
}

fn render_row<A>(ui: &mut egui::Ui, entry: &Entry<A>, selected: bool, style: &Style) -> egui::Response {
    let desired = egui::vec2(ui.available_width(), style.row_height);
    let (rect, resp) = ui.allocate_exact_size(desired, egui::Sense::click());
    if selected {
        let fill = style.selected_fill.unwrap_or_else(|| ui.visuals().selection.bg_fill.gamma_multiply(0.4));
        ui.painter().rect_filled(rect, 3.0, fill);
    }
    let inner = rect.shrink2(egui::vec2(8.0, 2.0));
    let body = egui::TextStyle::Body.resolve(ui.style());
    let subtitle_font = egui::FontId {
        size: style.subtitle_size.unwrap_or_else(|| egui::TextStyle::Small.resolve(ui.style()).size),
        ..body.clone()
    };
    let text_color = style.text_color.unwrap_or_else(|| ui.visuals().text_color());
    let icon_color = style.icon_color.unwrap_or(text_color);
    let sub_color = style.subtitle_color.unwrap_or_else(|| ui.visuals().weak_text_color());

    let title_x = if let Some(icon) = entry.icon.as_deref() {
        let galley =
            ui.painter().layout_no_wrap(icon.to_owned(), egui::FontId::proportional(style.icon_size), icon_color);
        let pos = egui::pos2(inner.left(), inner.center().y - galley.size().y * 0.5);
        ui.painter().galley(pos, galley, icon_color);
        inner.left() + style.icon_gutter
    } else {
        inner.left()
    };

    let title_width_budget = inner.right() - title_x;
    let title_galley = layout_truncated(ui, entry.title.clone(), body.clone(), text_color, title_width_budget);
    let title_pos = egui::pos2(title_x, inner.center().y - title_galley.size().y * 0.5);
    let title_size = title_galley.size();
    ui.painter().galley(title_pos, title_galley, text_color);

    if let Some(sub) = entry.subtitle.as_deref() {
        let sub_x = title_x + title_size.x + style.subtitle_spacing;
        let sub_budget = inner.right() - sub_x;
        if sub_budget > 0.0 {
            let sub_galley = layout_truncated(ui, sub.to_owned(), subtitle_font, sub_color, sub_budget);
            let sub_pos = egui::pos2(sub_x, inner.center().y - sub_galley.size().y * 0.5);
            ui.painter().galley(sub_pos, sub_galley, sub_color);
        }
    }
    resp
}

/// Lay out `text` in `font`, clipped to one row of `max_width`
/// pixels. Overflow is replaced with an ellipsis character so long
/// titles or filesystem paths don't spill past the panel edge.
fn layout_truncated(
    ui: &egui::Ui,
    text: String,
    font: egui::FontId,
    color: Color32,
    max_width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(
        text,
        egui::text::TextFormat { font_id: font, color, ..Default::default() },
    );
    job.wrap = egui::epaint::text::TextWrapping::truncate_at_width(max_width.max(0.0));
    ui.painter().layout_job(job)
}
