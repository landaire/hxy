//! Framework-agnostic state and fuzzy filtering for a Cmd+P-style
//! command palette. Owns [`Entry`], [`State`], the pick/dismiss
//! outcome types, and the fuzzy matcher; rendering and input
//! handling live in the UI-framework-specific crate that wraps
//! this one (e.g. `egui-palette`).

#![forbid(unsafe_code)]

pub mod fuzzy;

pub use fuzzy::MatchResult;
pub use fuzzy::filter_and_sort;

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
    /// and writes it here before calling `show`. The widget
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
    /// the widget ignores the `completion_suggestion` the host
    /// staged so the user's next Backspace eats from their typed
    /// prefix instead of being intercepted by a re-rendered
    /// ghost. Private: the widget drives this latch only through
    /// [`Self::completion_dismissed`], [`Self::dismiss_completion`],
    /// and [`Self::rearm_completion`] so a host can't desync it from
    /// the frame it was computed for.
    completion_dismissed: bool,
    /// Snapshot of `query` from the previous frame, used by
    /// [`Self::query_changed_since_last_frame`] to detect edits and
    /// snap `selected` back to the top (best match), matching VS
    /// Code / Zed UX. Private: only that method may advance the
    /// snapshot, keeping it in sync with the comparison it guards.
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

    /// Returns `true` if `query` differs from the snapshot taken on
    /// the last call to this method, and advances the snapshot to
    /// match. Call once per frame before deriving frame-local state
    /// (e.g. resetting `selected`) from the comparison.
    pub fn query_changed_since_last_frame(&mut self) -> bool {
        if self.query != self.last_query {
            // Reuse the `last_query` buffer instead of allocating a
            // fresh String every time the query changes; the typical
            // edit tacks on or deletes a handful of bytes, so the
            // existing capacity will fit.
            self.last_query.clone_from(&self.query);
            true
        } else {
            false
        }
    }

    /// Whether the inline completion ghost is currently latched
    /// dismissed (see the field docs on the latch above).
    pub fn completion_dismissed(&self) -> bool {
        self.completion_dismissed
    }

    /// Latch the completion-ghost dismissal: the widget stops
    /// re-staging a suggestion until [`Self::rearm_completion`] is
    /// called.
    pub fn dismiss_completion(&mut self) {
        self.completion_dismissed = true;
    }

    /// Clear the completion-ghost dismissal latch, e.g. once the
    /// user resumes typing at the end of the query and a fresh
    /// suggestion becomes eligible again.
    pub fn rearm_completion(&mut self) {
        self.completion_dismissed = false;
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
    /// Optional keyboard-shortcut hint rendered right-aligned in a
    /// muted color (e.g. `cmd-z`, `ctrl-shift-v`). Consumers
    /// typically pass their framework's shortcut-formatting output
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

impl<A> Entry<A> {
    pub fn new(title: impl Into<String>, data: A) -> Self {
        Self { title: title.into(), subtitle: None, icon: None, shortcut: None, disabled: false, data }
    }

    pub fn with_subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    pub fn with_icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
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
/// the matching entry's `data`; [`Outcome::Dismissed`] fires when the
/// host reports the palette closed without a pick (e.g. `Esc` or a
/// click on the backdrop). `K` is the host framework's key type
/// (e.g. `egui::Key`).
pub enum Outcome<A, K> {
    Picked(A),
    /// The user dismissed the palette without picking an entry.
    /// Carries the cause so hosts can make context-aware decisions
    /// (e.g. pop one cascade level on Escape, fully close on
    /// backdrop click).
    Dismissed(DismissReason<K>),
}

/// What caused the palette to dismiss without a pick. `K` is the
/// host framework's key type (e.g. `egui::Key`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DismissReason<K> {
    /// One of the host's configured dismiss keys was pressed
    /// (typically `Escape`). Hosts running cascade-style modes
    /// typically intercept this to pop back one level instead of
    /// closing.
    Key(K),
    /// The user clicked outside the panel onto the dimmed backdrop.
    /// Usually treated as an explicit "fully close" intent regardless
    /// of cascade depth.
    Backdrop,
}
