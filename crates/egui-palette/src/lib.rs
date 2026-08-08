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
//! if let Some(Outcome::Picked { data: action, .. }) = egui_palette::show(ctx, &mut state, &entries, "Search...") {
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
use egui::Widget;

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
    /// When `true`, the widget passes entries through unchanged
    /// instead of fuzzy-matching against `query`. Host-supplied
    /// entries are already the thing the user will activate --
    /// useful for "query is the argument" modes (e.g. Go to offset)
    /// where the entry list is a single dynamically-built row and
    /// any attempt to fuzzy-filter by the raw argument string would
    /// hide the entry the moment it didn't happen to be a subsequence
    /// of the entry's human-readable title.
    pub bypass_filter: bool,
    /// Browser-URL-style ghost text shown after the user's `query`,
    /// pre-selected so the next keystroke either consumes a char
    /// of it (selection-replace with a matching char) or wipes it
    /// (typing a non-matching char or pressing Backspace). The
    /// host (re)computes this each frame from the current `query`
    /// and writes it here before calling [`show`]. The widget
    /// renders the buffer as `query + suggestion`, sets the
    /// selection over the suggestion portion, and -- on the next
    /// frame -- syncs `query` to whatever the user committed.
    /// Right-arrow / End / Tab commit the suggestion; any other
    /// edit replaces or shrinks it.
    pub completion_suggestion: Option<String>,
    /// Latched when the user explicitly rejects the inline ghost
    /// (Backspace deletes the selected suggestion, or the cursor
    /// moves off the end without typing). Stays set until the
    /// user types another char at the end of `query`. While set,
    /// `show` ignores the `completion_suggestion` the host
    /// staged so the user's next Backspace eats from their typed
    /// prefix instead of being intercepted by a re-rendered
    /// ghost.
    completion_dismissed: bool,
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
        self.bypass_filter = false;
        self.completion_suggestion = None;
        self.completion_dismissed = false;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.bypass_filter = false;
        self.completion_suggestion = None;
        self.completion_dismissed = false;
    }
}

/// Leading icon for a row. `Glyph` is a string drawn with egui's
/// text layout (single character or short codepoint sequence like a
/// phosphor or SF Symbols name). `Image` is anything egui's image
/// system can render -- a pre-loaded texture, raw bytes with a URI
/// key, or a registered URI handled by `egui_extras`. `Image`
/// variants render unmodified (no theme tint) so full-color app
/// icons retain their colors; tint glyphs by setting
/// `Style::icon_color`.
pub enum EntryIcon<'a> {
    Glyph(Cow<'a, str>),
    Image(egui::ImageSource<'a>),
}

/// One selectable row. `data` is returned verbatim in
/// [`Outcome::Picked`]; the crate doesn't care what it is.
pub struct Entry<'a, A> {
    pub title: String,
    pub subtitle: Option<String>,
    /// Optional leading icon (glyph or image). Rendered in a fixed-
    /// width gutter on the left of the row.
    pub icon: Option<EntryIcon<'a>>,
    /// Optional keyboard-shortcut hint rendered right-aligned in a
    /// muted color (e.g. `cmd-z`, `ctrl-shift-v`). Consumers
    /// typically pass [`egui::Context::format_shortcut`]'s output
    /// here so the palette advertises the same keys that trigger the
    /// action outside the palette.
    pub shortcut: Option<String>,
    /// `true` greys out the row and silently ignores Enter / clicks
    /// on it. Use for actions whose preconditions aren't met (e.g.
    /// "Browse VFS" on a file with no detected handler) so the user
    /// can see *why* the option exists without being able to invoke
    /// it into a no-op.
    pub disabled: bool,
    pub data: A,
}

impl<'a, A> Entry<'a, A> {
    pub fn new(title: impl Into<String>, data: A) -> Self {
        Self { title: title.into(), subtitle: None, icon: None, shortcut: None, disabled: false, data }
    }

    pub fn with_subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    /// Set a font-glyph icon (single character or short string).
    pub fn with_icon_glyph(mut self, s: impl Into<Cow<'a, str>>) -> Self {
        self.icon = Some(EntryIcon::Glyph(s.into()));
        self
    }

    /// Set an image icon from any source egui understands.
    pub fn with_icon_image(mut self, src: impl Into<egui::ImageSource<'a>>) -> Self {
        self.icon = Some(EntryIcon::Image(src.into()));
        self
    }

    /// Deprecated shim that calls `with_icon_glyph`. Kept so 0.4
    /// callers compile during migration.
    #[deprecated(note = "use with_icon_glyph or with_icon_image")]
    pub fn with_icon(self, icon: impl Into<String>) -> Self {
        let s: String = icon.into();
        self.with_icon_glyph(Cow::Owned(s))
    }

    pub fn with_shortcut(mut self, shortcut: impl Into<String>) -> Self {
        self.shortcut = Some(shortcut.into());
        self
    }

    pub fn with_disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

/// What happened this frame. [`Outcome::Picked`] carries a *clone* of
/// the matching entry's `data`; [`Outcome::Closed`] fires on `Esc`
/// or a click on the backdrop.
pub enum Outcome<A> {
    /// User activated an entry. `modifiers` carries the modifier
    /// state of the activating key combo (matched against
    /// [`Style::activation_modifiers`]), so hosts can route the same
    /// row to different actions based on Cmd / Shift / etc.
    Picked { data: A, modifiers: egui::Modifiers },
    /// The user pressed [`Style::sub_action_shortcut`] (default
    /// `Cmd+K`) on a selected row, requesting a per-row actions
    /// sub-palette instead of activating the row outright.
    SubAction { data: A },
    /// The user dismissed the palette without picking an entry.
    /// Carries the cause so hosts can make context-aware decisions
    /// (e.g. pop one cascade level on Escape, fully close on
    /// backdrop click).
    Dismissed(DismissReason),
}

/// What caused the palette to dismiss without a pick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DismissReason {
    /// One of [`Style::dismiss_keys`] was pressed (defaults to
    /// `Escape`). Hosts running cascade-style modes typically
    /// intercept this to pop back one level instead of closing.
    Key(egui::Key),
    /// The user clicked outside the panel onto the dimmed backdrop.
    /// Usually treated as an explicit "fully close" intent regardless
    /// of cascade depth.
    Backdrop,
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

/// How the result list scrolls to keep the keyboard-selected row on
/// screen when arrow keys move the selection out of view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ScrollToSelection {
    /// Re-center the selected row in the viewport whenever it moves
    /// offscreen. The list jumps by roughly half a viewport.
    #[default]
    Center,
    /// Scroll the minimum needed to reveal the row at whichever edge it
    /// left, so the selection pins to the top/bottom edge and the list
    /// shifts one row at a time. Matches macOS-style launchers (Raycast,
    /// Spotlight, the gpui client).
    Nearest,
}

impl ScrollToSelection {
    /// The `align` argument for [`egui::Response::scroll_to_me`].
    fn align(self) -> Option<egui::Align> {
        match self {
            Self::Center => Some(egui::Align::Center),
            Self::Nearest => None,
        }
    }
}

/// Everything tweakable about the palette. All `Option<Color32>` /
/// `Option<Stroke>` fields use egui's theme visuals when `None`, so
/// the defaults track light-mode / dark-mode switches automatically.
#[derive(Clone)]
pub struct Style {
    // ---- Position ----
    pub anchor: Anchor,

    /// How the list scrolls to keep the keyboard-selected row visible.
    /// Defaults to [`ScrollToSelection::Center`].
    pub scroll_to_selection: ScrollToSelection,

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
    /// Per-row internal padding: x is the space reserved on each
    /// side of the row's content (icon, title, subtitle, shortcut);
    /// y trims the selection-fill rectangle so it doesn't hug the
    /// row height.
    pub row_padding: egui::Vec2,
    pub corner_radius: egui::CornerRadius,
    /// Hard ceiling for the result list height. Also the value used
    /// when the viewport is too short to derive one automatically.
    pub list_max_height: f32,
    pub list_min_height: f32,
    /// Subtracted from viewport height when sizing the scroll list
    /// (padding reserved for the text input + margins).
    pub row_reserve: f32,
    /// When `true` (default), the panel sizes its result list to the
    /// current row count -- it shrinks as the filter narrows and
    /// grows back (up to [`Self::list_max_height`]) as matches come
    /// back. When `false`, the list always claims the full
    /// `list_max_height` regardless of content, which hosts with
    /// "fixed-size palette" UX may prefer because the panel doesn't
    /// twitch as the user types.
    pub list_shrink_to_fit: bool,

    // ---- Colors (None = follow egui::Visuals) ----
    /// Full-viewport overlay painted behind the panel. `None` means
    /// no backdrop at all (the palette floats on top of unmodified
    /// app UI, e.g. for inline / always-open palettes).
    pub backdrop: Option<Color32>,
    /// Panel background fill. `None` uses [`egui::Frame::popup`]'s
    /// theme-derived color.
    pub panel_fill: Option<Color32>,
    /// Panel outline. `None` leaves [`egui::Frame::popup`]'s default.
    pub panel_stroke: Option<Stroke>,
    /// Fill painted behind the currently-selected row. `None`
    /// derives from `visuals.selection.bg_fill` with 0.4 opacity so
    /// it reads on both light and dark themes.
    pub selected_fill: Option<Color32>,
    /// Corner radius of the selected-row fill. `None` uses a small
    /// 3px rounding.
    pub selected_corner_radius: Option<f32>,
    /// Color of the entry title and the icon. `None` uses
    /// `visuals.text_color()`.
    pub text_color: Option<Color32>,
    /// Color of the entry subtitle. `None` uses
    /// `visuals.weak_text_color()`.
    pub subtitle_color: Option<Color32>,
    /// Font size of the entry title. `None` uses
    /// [`egui::TextStyle::Body`].
    pub title_font_size: Option<f32>,
    /// Font size used for the subtitle. `None` falls back to the
    /// size of [`egui::TextStyle::Small`] (noticeably smaller than
    /// the title so a long path reads as secondary).
    pub subtitle_size: Option<f32>,
    /// Font size of the query input text. `None` uses
    /// [`egui::TextStyle::Body`], matching the title size.
    pub input_font_size: Option<f32>,
    /// Whether the query input draws its own background frame and
    /// focus outline. `None` leaves egui's default (framed); `false`
    /// makes the input frameless so it blends into the panel.
    pub input_frame: Option<bool>,
    /// Color of the icon glyph. `None` uses [`Self::text_color`] so
    /// icons and titles match unless explicitly split.
    pub icon_color: Option<Color32>,
    /// Color used to mark characters in the title / subtitle that
    /// the fuzzy matcher hit for the current query. Defaults to
    /// `visuals.selection.stroke.color` so it stands out against
    /// both title and subtitle baselines without a hardcoded hue.
    pub match_color: Option<Color32>,

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
    /// Modifier combos that activate the selected row. The widget
    /// iterates this list in order on each frame; the first combo
    /// whose key state matches consumes the Enter press and is
    /// reported back to the host. Order most-specific first: egui's
    /// `consume_key(Cmd, Enter)` returns true when `Cmd+Shift+Enter`
    /// is pressed, so a list of `[Cmd, Shift]` will silently misroute
    /// Cmd+Shift+Enter to the Cmd handler. List
    /// `Cmd|Shift` before `Cmd` and `Shift`, and `NONE` last.
    pub activation_modifiers: Cow<'static, [egui::Modifiers]>,
    /// Keyboard shortcut that opens a per-row "actions" sub-palette.
    /// Pressed while a row is selected, the widget emits
    /// `Outcome::SubAction { data }` instead of `Picked`. `None`
    /// disables sub-actions entirely. Default `Cmd+K`.
    pub sub_action_shortcut: Option<egui::KeyboardShortcut>,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            anchor: Anchor::default(),
            scroll_to_selection: ScrollToSelection::default(),
            min_width: 360.0,
            max_width: 560.0,
            width_fraction: 0.38,
            row_height: 22.0,
            icon_size: 14.0,
            icon_gutter: 20.0,
            subtitle_spacing: 8.0,
            inner_margin: egui::Margin::symmetric(12, 10),
            row_padding: egui::vec2(4.0, 2.0),
            corner_radius: egui::CornerRadius::same(8),
            // Cap the visible list around ~12 rows before scrolling
            // kicks in. Matches the "Cmd+P shows a dozen matches"
            // feel of VS Code / Zed / Sublime; hosts that want the
            // palette to expand further can raise this.
            list_max_height: 300.0,
            list_min_height: 120.0,
            row_reserve: 96.0,
            list_shrink_to_fit: true,
            backdrop: Some(Color32::from_black_alpha(120)),
            panel_fill: None,
            panel_stroke: None,
            selected_fill: None,
            selected_corner_radius: None,
            text_color: None,
            subtitle_color: None,
            title_font_size: None,
            subtitle_size: None,
            input_font_size: None,
            input_frame: None,
            icon_color: None,
            match_color: None,
            close_on_backdrop_click: true,
            consume_nav_keys: true,
            matcher: MatcherConfig::DEFAULT,
            case_matching: CaseMatching::Smart,
            normalization: Normalization::Smart,
            dismiss_keys: Cow::Borrowed(&[egui::Key::Escape]),
            activation_modifiers: Cow::Borrowed(&[
                egui::Modifiers { command: true, shift: true, ..egui::Modifiers::NONE },
                egui::Modifiers { command: true, ..egui::Modifiers::NONE },
                egui::Modifiers { shift: true, ..egui::Modifiers::NONE },
                egui::Modifiers::NONE,
            ]),
            sub_action_shortcut: Some(egui::KeyboardShortcut::new(
                egui::Modifiers::COMMAND,
                egui::Key::K,
            )),
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

    /// Override the color of the semi-transparent backdrop, or
    /// pass `None` to disable the backdrop entirely.
    pub fn backdrop_fill(mut self, fill: Option<Color32>) -> Self {
        self.backdrop = fill;
        self
    }

    /// Convenience: override panel fill + outline together.
    pub fn panel_colors(mut self, fill: Color32, stroke: Stroke) -> Self {
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
pub fn show<'a, A: Clone>(
    ctx: &egui::Context,
    state: &mut State,
    entries: &[Entry<'a, A>],
    hint: &str,
) -> Option<Outcome<A>> {
    show_with_style(ctx, state, entries, hint, &Style::default())
}

/// Variant of [`show`] that takes an explicit [`Style`].
pub fn show_with_style<'a, A: Clone>(
    ctx: &egui::Context,
    state: &mut State,
    entries: &[Entry<'a, A>],
    hint: &str,
    style: &Style,
) -> Option<Outcome<A>> {
    show_with_style_inner(ctx, state, entries, hint, style, None)
}

/// Render the palette with a footer slot. The provided closure is
/// invoked at the bottom of the panel inside the same `Frame` as
/// the result list, separated by a thin separator. Use this for
/// action hints, status text, or any per-frame footer content the
/// host wants to surface.
pub fn show_with_footer<'a, A: Clone>(
    ctx: &egui::Context,
    state: &mut State,
    entries: &[Entry<'a, A>],
    hint: &str,
    style: &Style,
    footer: &dyn Fn(&mut egui::Ui),
) -> Option<Outcome<A>> {
    show_with_style_inner(ctx, state, entries, hint, style, Some(footer))
}

fn show_with_style_inner<'a, A: Clone>(
    ctx: &egui::Context,
    state: &mut State,
    entries: &[Entry<'a, A>],
    hint: &str,
    style: &Style,
    footer: Option<&dyn Fn(&mut egui::Ui)>,
) -> Option<Outcome<A>> {
    if !state.open {
        return None;
    }

    // Drain matching key-press events so downstream handlers don't
    // also react to the same press (e.g. clearing a hex-editor
    // selection when the user hit Esc only to dismiss the palette).
    // The first matching key wins -- callers get its identity in
    // `DismissReason::Key` so they can distinguish bindings if they
    // configured more than one.
    //
    // Don't return early here even if a dismiss key fired: the host
    // might react by switching to a sibling mode (cascade pop-back)
    // and then re-render the palette next frame. If we skipped
    // painting *this* frame the user would see one frame of
    // background showing through -- a visible flash. Painting the
    // current state and returning the outcome at the end of `show`
    // keeps the panel on screen continuously across mode swaps.
    let dismissed_key = ctx.input_mut(|i| {
        let mut found: Option<egui::Key> = None;
        i.events.retain(|event| {
            let egui::Event::Key { key, pressed: true, repeat: false, .. } = event else {
                return true;
            };
            if found.is_none() && style.dismiss_keys.iter().any(|k| k == key) {
                found = Some(*key);
                return false;
            }
            true
        });
        found
    });

    let filtered = if state.bypass_filter {
        // Host wants every entry shown in declaration order; the
        // query is an argument being typed, not a filter. Each
        // result has empty `match_indices` so `layout_highlighted`
        // won't paint spurious highlights on non-matching chars.
        (0..entries.len()).map(|index| fuzzy::MatchResult { index, match_indices: Vec::new() }).collect()
    } else {
        fuzzy::filter_and_sort(&state.query, entries, &style.matcher, style.case_matching, style.normalization, |e| {
            match &e.subtitle {
                Some(sub) => std::borrow::Cow::Owned(format!("{} {}", e.title, sub)),
                None => std::borrow::Cow::Borrowed(e.title.as_str()),
            }
        })
    };
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
    let mut pick_modifiers: Option<egui::Modifiers> = None;
    let mut sub_action_idx: Option<usize> = None;
    let mut selection_changed_by_kbd = false;
    if style.consume_nav_keys {
        ctx.input_mut(|i| {
            if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) && !filtered.is_empty() {
                state.selected = (state.selected + 1) % filtered.len();
                selection_changed_by_kbd = true;
            }
            if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) && !filtered.is_empty() {
                state.selected = (state.selected + filtered.len() - 1) % filtered.len();
                selection_changed_by_kbd = true;
            }
            for modifiers in style.activation_modifiers.iter() {
                if i.consume_key(*modifiers, egui::Key::Enter) && !filtered.is_empty() {
                    picked_idx = Some(state.selected);
                    pick_modifiers = Some(*modifiers);
                    break;
                }
            }
            if let Some(shortcut) = &style.sub_action_shortcut {
                if i.consume_shortcut(shortcut) && !filtered.is_empty() {
                    sub_action_idx = Some(state.selected);
                }
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
            return Some(Outcome::Dismissed(DismissReason::Backdrop));
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

    // Use `egui::Area` (single, stable id) so the layer is
    // registered with an `AreaState` -- input hit-testing
    // (`layer_id_at`) skips layers that have no AreaState, which
    // breaks `ScrollArea`'s wheel-event check (it calls
    // `ui.rect_contains_pointer`, which goes through `layer_id_at`).
    //
    // The trap with `Area` is that it caches `state.size =
    // content_ui.min_size()` across frames and uses it as the next
    // frame's `max_rect`. If the result list shrinks to a few rows
    // and the user clears the filter, the cached small `max_rect`
    // pins the inner `ScrollArea` viewport to a tiny height even
    // though the row count exploded. Calling `ui.set_max_height`
    // first thing inside the closure overrides that cached height
    // for the current frame, so the inner `ScrollArea` always sees
    // enough room to claim its full viewport. The Area's *visual*
    // size still shrinks with content because `ScrollArea` is set
    // to `auto_shrink([false, true])`.
    let palette_id = egui::Id::new("egui_palette_panel");
    let area_inner_height = list_max_height + style.row_reserve;
    let area_response = egui::Area::new(palette_id)
        .order(egui::Order::Foreground)
        .fixed_pos(egui::pos2(panel_x, panel_y))
        .interactable(true)
        .default_size(egui::vec2(panel_width, area_inner_height))
        .show(ctx, |ui| {
            // Break the cached-size trap: even if last frame's content
            // was tiny, give the inner layout enough vertical room to
            // re-grow this frame.
            ui.set_max_height(area_inner_height);
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
                // `panel_width` is the intended outer width; the Frame adds
                // `inner_margin` around this content, so constrain the content to
                // `panel_width - horizontal margin` or the panel draws wider than
                // requested and overflows its host on the right.
                let content_width =
                    panel_width - style.inner_margin.leftf() - style.inner_margin.rightf();
                ui.set_min_width(content_width);
                ui.set_max_width(content_width);

                // Inline ghost completion: when the host has
                // staged a suggestion -- and the user hasn't
                // explicitly dismissed completion via Backspace
                // / mid-click -- render the buffer as
                // `query + suggestion` with the suggestion
                // portion pre-selected. egui's TextEdit handles
                // selection-replacement natively, so the next
                // keystroke either consumes one char of the
                // suggestion (matching keystroke) or wipes it
                // (non-matching or Backspace). Right-arrow / End
                // collapse the selection to its end, committing
                // the suggestion into `query`.
                //
                // The `completion_dismissed` latch breaks the
                // loop where re-staging a suggestion every frame
                // would intercept every Backspace -- the user
                // could never delete past the ghost into their
                // own text.
                let staged = state.completion_suggestion.take();
                let suggestion = if state.completion_dismissed { None } else { staged };
                let display_buffer: String = match &suggestion {
                    Some(s) => format!("{}{s}", state.query),
                    None => state.query.clone(),
                };
                let text_edit_id = egui::Id::new("egui_palette_text_edit");
                let suggestion_range: Option<(usize, usize)> = suggestion.as_ref().map(|s| {
                    let start = state.query.chars().count();
                    let end = start + s.chars().count();
                    (start, end)
                });
                if let Some((start, end)) = suggestion_range {
                    let mut tx_state = egui::TextEdit::load_state(ui.ctx(), text_edit_id).unwrap_or_default();
                    tx_state.cursor.set_char_range(Some(egui::text_selection::CCursorRange::two(
                        egui::text::CCursor::new(start),
                        egui::text::CCursor::new(end),
                    )));
                    egui::TextEdit::store_state(ui.ctx(), text_edit_id, tx_state);
                }
                let prev_query_chars = state.query.chars().count();
                let mut buffer = display_buffer.clone();
                let mut text_edit = egui::TextEdit::singleline(&mut buffer)
                    .id(text_edit_id)
                    .hint_text(hint)
                    .desired_width(f32::INFINITY);
                if let Some(size) = style.input_font_size {
                    text_edit = text_edit.font(egui::FontId::proportional(size));
                }
                if style.input_frame == Some(false) {
                    text_edit = text_edit.frame(egui::Frame::NONE);
                }
                let resp = ui.add(text_edit);
                if state.pending_focus {
                    resp.request_focus();
                    state.pending_focus = false;
                } else if !resp.has_focus() {
                    resp.request_focus();
                }
                // Reconcile post-frame buffer / cursor with the
                // intended query, and update the
                // `completion_dismissed` latch.
                let post_state = egui::TextEdit::load_state(ui.ctx(), text_edit_id);
                let buffer_chars = buffer.chars().count();
                let display_chars = display_buffer.chars().count();
                state.query = if buffer != display_buffer {
                    // Edit happened (typed, deleted, pasted).
                    let backspaced_suggestion = suggestion.is_some() && buffer == state.query;
                    if backspaced_suggestion {
                        // User explicitly cleared the ghost.
                        // Latch dismissal so the next Backspace
                        // eats their typed prefix instead.
                        state.completion_dismissed = true;
                    } else if buffer_chars > prev_query_chars {
                        // Net growth -- user typed at the end
                        // (or selection-replaced with a matching
                        // char that grew the buffer). Re-arm
                        // completion.
                        state.completion_dismissed = false;
                    }
                    buffer
                } else if let Some((start, end)) = suggestion_range {
                    // Buffer unchanged, but a suggestion was
                    // pending. Distinguish (a) "user did
                    // nothing" -- selection still over the
                    // suggestion, (b) "user committed" -- cursor
                    // collapsed at the end of the buffer
                    // (Right/End), and (c) "user navigated
                    // mid-buffer" -- cursor collapsed somewhere
                    // else. Only the third case latches
                    // dismissal; if the cursor info is
                    // unavailable for some reason (no stored
                    // state, focus not yet established) we keep
                    // the suggestion alive rather than
                    // accidentally dismissing on the very first
                    // frame the ghost appears.
                    if let Some(r) = post_state.as_ref().and_then(|st| st.cursor.char_range()) {
                        let lo = r.primary.index.min(r.secondary.index);
                        let hi = r.primary.index.max(r.secondary.index);
                        let still_selected = lo == egui::text::CharIndex(start)
                            && hi == egui::text::CharIndex(end);
                        let committed = r.is_empty()
                            && r.primary.index == egui::text::CharIndex(display_chars);
                        if still_selected {
                            state.query.clone()
                        } else if committed {
                            buffer
                        } else {
                            state.completion_dismissed = true;
                            state.query.clone()
                        }
                    } else {
                        state.query.clone()
                    }
                } else {
                    // No suggestion was rendered this frame; just
                    // sync the buffer. Dismissal flips off as soon
                    // as the user types more (length grows).
                    if buffer_chars > prev_query_chars {
                        state.completion_dismissed = false;
                    }
                    buffer
                };

                ui.add_space(6.0);
                // Sync selection from hover only while the pointer is
                // actually moving. Without this gate, opening the
                // palette with the cursor already over the list area
                // would slam `selected` to whatever row it started on
                // -- often the bottom row the user was hovering when
                // they hit Cmd+P -- instead of the intended row 0.
                let pointer_moving = ui.ctx().input(|i| i.pointer.delta() != egui::Vec2::ZERO);
                // One ScrollArea handles both shrink and scroll cases. When
                // `list_shrink_to_fit` is true (default), `auto_shrink` shrinks
                // the viewport vertically to content size when it fits and caps
                // at `list_max_height` (showing a scrollbar) when it doesn't.
                // When false, vertical shrink is disabled so the list always
                // claims the full `list_max_height` -- a fixed-size palette that
                // fills its host regardless of row count (matching the doc).
                egui::ScrollArea::vertical()
                    .max_height(list_max_height)
                    .auto_shrink([false, style.list_shrink_to_fit])
                    .show(ui, |ui| {
                    for (row, hit) in filtered.iter().enumerate() {
                        let entry = &entries[hit.index];
                        let selected = row == state.selected;
                        // Salt each row's widget id with its stable entry index.
                        // Without this the row's click-sense rect takes an
                        // auto-generated id off the sequential counter, which
                        // shifts between egui's sizing and render passes in a
                        // tall list and spams "changed id between passes".
                        // Salt each row's widget id with its stable entry index.
                        // Without this the row's click-sense rect takes an
                        // auto-generated id off the sequential counter; if an
                        // upstream widget count shifts between egui's sizing and
                        // render passes (e.g. the input's ghost completion) every
                        // row's id moves and egui spams "changed id between
                        // passes". A stable per-row id is immune to that shift.
                        let resp = ui
                            .push_id(hit.index, |ui| {
                                render_row(ui, entry, selected, style, &hit.match_indices)
                            })
                            .inner;
                        if resp.clicked() {
                            picked_idx = Some(row);
                        }
                        if resp.hovered() && pointer_moving {
                            state.selected = row;
                        }
                        // Keep the keyboard-driven selection on screen.
                        // Skip on hover-driven changes -- those are
                        // already inside the viewport by definition,
                        // and triggering scroll on hover causes the
                        // list to drift under the cursor.
                        if selected && selection_changed_by_kbd {
                            resp.scroll_to_me(style.scroll_to_selection.align());
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
                if let Some(footer_fn) = footer {
                    ui.separator();
                    footer_fn(ui);
                }
            });
        });
    let _ = area_response;

    // Pick wins over dismiss: hitting Enter on a row already
    // commits the action, even if some unrelated dismiss key was in
    // the same input batch (rare in practice but cheap to specify).
    if let Some(row) = picked_idx
        && let Some(hit) = filtered.get(row)
        && !entries[hit.index].disabled
    {
        return Some(Outcome::Picked {
            data: entries[hit.index].data.clone(),
            modifiers: pick_modifiers.unwrap_or(egui::Modifiers::NONE),
        });
    }
    if let Some(row) = sub_action_idx
        && let Some(hit) = filtered.get(row)
        && !entries[hit.index].disabled
    {
        return Some(Outcome::SubAction { data: entries[hit.index].data.clone() });
    }
    if let Some(key) = dismissed_key {
        return Some(Outcome::Dismissed(DismissReason::Key(key)));
    }
    None
}

fn render_row<A>(
    ui: &mut egui::Ui,
    entry: &Entry<'_, A>,
    selected: bool,
    style: &Style,
    match_indices: &[u32],
) -> egui::Response {
    let desired = egui::vec2(ui.available_width(), style.row_height);
    let (rect, resp) = ui.allocate_exact_size(desired, egui::Sense::click());
    if selected {
        let fill = style.selected_fill.unwrap_or_else(|| ui.visuals().selection.bg_fill.gamma_multiply(0.4));
        ui.painter().rect_filled(rect, style.selected_corner_radius.unwrap_or(3.0), fill);
    }
    let inner = rect.shrink2(style.row_padding);
    let mut body = egui::TextStyle::Body.resolve(ui.style());
    if let Some(size) = style.title_font_size {
        body.size = size;
    }
    let subtitle_font = egui::FontId {
        size: style.subtitle_size.unwrap_or_else(|| egui::TextStyle::Small.resolve(ui.style()).size),
        ..body.clone()
    };
    let text_color = style.text_color.unwrap_or_else(|| ui.visuals().text_color());
    let icon_color = style.icon_color.unwrap_or(text_color);
    let sub_color = style.subtitle_color.unwrap_or_else(|| ui.visuals().weak_text_color());
    let match_color = style.match_color.unwrap_or_else(|| ui.visuals().selection.stroke.color);
    // Dim every painted color uniformly when the row is disabled so
    // the disabled state is visually obvious without re-themeing
    // each text run individually.
    let (text_color, icon_color, sub_color, match_color) = if entry.disabled {
        let dim = |c: Color32| c.gamma_multiply(0.45);
        (dim(text_color), dim(icon_color), dim(sub_color), dim(match_color))
    } else {
        (text_color, icon_color, sub_color, match_color)
    };

    // Split the fuzzy-matcher's char indices (into the combined
    // "title subtitle" haystack) into title-local and subtitle-local
    // index lists so each run of marked characters highlights the
    // right piece of text.
    let title_char_len = entry.title.chars().count() as u32;
    let separator_len = if entry.subtitle.is_some() { 1 } else { 0 };
    let mut title_indices: Vec<u32> = Vec::new();
    let mut subtitle_indices: Vec<u32> = Vec::new();
    for &i in match_indices {
        if i < title_char_len {
            title_indices.push(i);
        } else if entry.subtitle.is_some() {
            let sub_i = i.saturating_sub(title_char_len + separator_len);
            subtitle_indices.push(sub_i);
        }
    }

    // Lay out the shortcut hint first so we can reserve its width on
    // the right edge and trim the title / subtitle budget to match.
    // Rendered in the subtitle font + color to match Zed's muted
    // cmd-binding style.
    let shortcut_galley = entry
        .shortcut
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| layout_truncated(ui, s.to_owned(), subtitle_font.clone(), sub_color, inner.width() * 0.5));
    let shortcut_reserved = shortcut_galley.as_ref().map(|g| g.size().x + style.subtitle_spacing).unwrap_or(0.0);
    let content_right = inner.right() - shortcut_reserved;

    let title_x = match entry.icon.as_ref() {
        None => inner.left(),
        Some(EntryIcon::Glyph(s)) => {
            let galley = ui.painter().layout_no_wrap(
                s.as_ref().to_owned(),
                egui::FontId::proportional(style.icon_size),
                icon_color,
            );
            let pos = egui::pos2(inner.left(), inner.center().y - galley.size().y * 0.5);
            ui.painter().galley(pos, galley, icon_color);
            inner.left() + style.icon_gutter
        }
        Some(EntryIcon::Image(src)) => {
            let icon_rect = egui::Rect::from_center_size(
                egui::pos2(inner.left() + style.icon_size * 0.5, inner.center().y),
                egui::vec2(style.icon_size, style.icon_size),
            );
            let mut child = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(icon_rect)
                    .layout(egui::Layout::centered_and_justified(egui::Direction::TopDown)),
            );
            egui::Image::new(src.clone())
                .fit_to_exact_size(egui::vec2(style.icon_size, style.icon_size))
                .ui(&mut child);
            inner.left() + style.icon_gutter
        }
    };

    let title_width_budget = content_right - title_x;
    let title_galley =
        layout_highlighted(ui, &entry.title, body.clone(), text_color, match_color, &title_indices, title_width_budget);
    let title_pos = egui::pos2(title_x, inner.center().y - title_galley.size().y * 0.5);
    let title_size = title_galley.size();
    ui.painter().galley(title_pos, title_galley, text_color);

    if let Some(sub) = entry.subtitle.as_deref() {
        let sub_x = title_x + title_size.x + style.subtitle_spacing;
        let sub_budget = content_right - sub_x;
        if sub_budget > 0.0 {
            let sub_galley =
                layout_highlighted(ui, sub, subtitle_font, sub_color, match_color, &subtitle_indices, sub_budget);
            let sub_pos = egui::pos2(sub_x, inner.center().y - sub_galley.size().y * 0.5);
            ui.painter().galley(sub_pos, sub_galley, sub_color);
        }
    }

    if let Some(galley) = shortcut_galley {
        let size = galley.size();
        let pos = egui::pos2(inner.right() - size.x, inner.center().y - size.y * 0.5);
        ui.painter().galley(pos, galley, sub_color);
    }
    resp
}

/// Lay out `text` with matched character positions painted in
/// `match_color` and everything else in `base_color`. Char-based
/// positions are into `text` directly (the caller is responsible
/// for mapping the fuzzy matcher's haystack indices down to per-
/// field indices). Truncates at `max_width` with an ellipsis like
/// [`layout_truncated`].
fn layout_highlighted(
    ui: &egui::Ui,
    text: &str,
    font: egui::FontId,
    base_color: Color32,
    match_color: Color32,
    match_indices: &[u32],
    max_width: f32,
) -> std::sync::Arc<egui::Galley> {
    if match_indices.is_empty() {
        return layout_truncated(ui, text.to_owned(), font, base_color, max_width);
    }
    let mut job = egui::text::LayoutJob::default();
    let mut in_match = false;
    let mut run_start_byte = 0usize;
    // Walk the text char-by-char, emitting a section whenever the
    // char's highlight state changes. Indices are sorted + deduped
    // by the matcher so a single linear scan suffices.
    let mut match_cursor = 0usize;
    for (char_idx, (byte_idx, ch)) in text.char_indices().enumerate() {
        let char_idx_u32 = char_idx as u32;
        while match_cursor < match_indices.len() && match_indices[match_cursor] < char_idx_u32 {
            match_cursor += 1;
        }
        let this_matches = match_cursor < match_indices.len() && match_indices[match_cursor] == char_idx_u32;
        if char_idx == 0 {
            in_match = this_matches;
            run_start_byte = byte_idx;
            continue;
        }
        if this_matches != in_match {
            let color = if in_match { match_color } else { base_color };
            job.append(
                &text[run_start_byte..byte_idx],
                0.0,
                egui::text::TextFormat { font_id: font.clone(), color, ..Default::default() },
            );
            run_start_byte = byte_idx;
            in_match = this_matches;
        }
        let _ = ch;
    }
    // Trailing run.
    if run_start_byte < text.len() {
        let color = if in_match { match_color } else { base_color };
        job.append(&text[run_start_byte..], 0.0, egui::text::TextFormat { font_id: font, color, ..Default::default() });
    }
    job.wrap = egui::epaint::text::TextWrapping::truncate_at_width(max_width.max(0.0));
    ui.painter().layout_job(job)
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

#[cfg(test)]
mod outcome_tests {
    use super::*;
    use egui::{Event, Key, Modifiers, RawInput};

    fn press(modifiers: Modifiers, key: Key) -> Event {
        Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    fn drive(input_events: Vec<Event>, style: &Style) -> Option<Outcome<&'static str>> {
        let ctx = egui::Context::default();
        let mut state = State::default();
        state.open();
        let entries = vec![Entry::new("only", "payload")];
        let mut outcome = None;
        let raw = RawInput { events: input_events, ..Default::default() };
        let mut out = ctx.run_ui(raw, |ui| {
            outcome = show_with_style(ui.ctx(), &mut state, &entries, "", style);
        });
        // Headless test: nothing renders, so discard texture deltas
        // instead of applying them (egui panics on silent drops).
        out.textures_delta.clear();
        outcome
    }

    #[test]
    fn cmd_shift_enter_does_not_match_cmd_enter() {
        let style = Style::default();
        let modifiers = Modifiers { command: true, shift: true, ..Modifiers::NONE };
        let out = drive(vec![press(modifiers, Key::Enter)], &style);
        match out {
            Some(Outcome::Picked { modifiers: m, .. }) => {
                assert!(m.command && m.shift, "expected Cmd+Shift, got {:?}", m);
            }
            _ => panic!("expected Picked with Cmd+Shift"),
        }
    }

    #[test]
    fn plain_enter_picks_with_none_modifiers() {
        let style = Style::default();
        let out = drive(vec![press(Modifiers::NONE, Key::Enter)], &style);
        match out {
            Some(Outcome::Picked { modifiers: m, .. }) => assert_eq!(m, Modifiers::NONE),
            _ => panic!("expected Picked NONE"),
        }
    }

    #[test]
    fn cmd_k_emits_sub_action() {
        let style = Style::default();
        let out = drive(vec![press(Modifiers::COMMAND, Key::K)], &style);
        match out {
            Some(Outcome::SubAction { data }) => assert_eq!(data, "payload"),
            _ => panic!("expected SubAction"),
        }
    }

    #[test]
    fn entry_supports_glyph_and_image_icons() {
        let mut e: Entry<&'static str> = Entry::new("title", "data");
        assert!(e.icon.is_none());

        e = e.with_icon_glyph("X");
        match e.icon.as_ref().unwrap() {
            EntryIcon::Glyph(s) => assert_eq!(s.as_ref(), "X"),
            _ => panic!("expected glyph"),
        }

        let bytes = egui::load::Bytes::Static(b"fake-png");
        let src = egui::ImageSource::Bytes { uri: "test://1".into(), bytes };
        let e2: Entry<&'static str> = Entry::new("t2", "d2").with_icon_image(src);
        assert!(matches!(e2.icon.as_ref(), Some(EntryIcon::Image(_))));
    }

    #[test]
    fn render_row_handles_all_icon_variants() {
        let ctx = egui::Context::default();
        let mut state = State::default();
        state.open();
        let entries: Vec<Entry<'static, &'static str>> = vec![
            Entry::new("no-icon", "a"),
            Entry::new("glyph", "b").with_icon_glyph("X"),
            Entry::new("image", "c").with_icon_image(egui::ImageSource::Bytes {
                uri: "test://img".into(),
                bytes: egui::load::Bytes::Static(b"fake"),
            }),
        ];
        let raw = egui::RawInput::default();
        let mut out = ctx.run_ui(raw, |ui| {
            let _ = show_with_style(ui.ctx(), &mut state, &entries, "", &Style::default());
        });
        out.textures_delta.clear();
    }
}
