# M0: Extract hxy-editor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract the framework-agnostic hex-edit model (EditState, HexEditor, vim state machine, input dispatch) out of `hxy-view` into a new `hxy-editor` crate, and the fuzzy-filter/palette state out of `egui-palette` into a new `palette-core` crate, with the egui side refactored onto them and all existing tests green.

**Architecture:** `hxy-editor` owns HexEditor and consumes a framework-neutral `InputEvent` stream via an `InputFilter` (decides per-event consumption) plus `apply_input` (mutates the editor, returns `Effect`s such as clipboard writes). `hxy-view` keeps every egui-facing piece: event translation, an extension trait providing the old `view()` / `handle_input()` / `on_response()` methods, and all rendering. Code is COPIED into the new crate first, then the egui crate is switched over and its copy deleted, so the workspace builds at every commit.

**Tech Stack:** Rust 2024, cargo workspace, jj (colocated), suture, hxy-core, nucleo_matcher, egui 0.34 (adapter side only), egui_kittest (existing interaction tests).

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-24-gpui-port-design.md`. M0 gate: "existing tests and egui_kittest interaction tests green; egui app behavior unchanged."
- All work happens in a new jj workspace (Task 0). Repo is jj-colocated: use `jj`, never `git`, and never interactive subcommands.
- Conventional commit style; NO `Co-Authored-By` or AI attribution trailers.
- ASCII only in code, comments, and commit messages. No emdash, arrows, ellipsis, emoji, or banner/separator comments.
- Typed errors only (`thiserror`); no `anyhow`, no matching on Display strings.
- Newtypes and enums over primitives/bools where new API is added; scrutinize any new `unwrap_or*`.
- `hxy-view` public API must remain source-compatible for `crates/hxy` except that `view()` / `handle_input()` / `on_response()` move to an importable `HexEditorExt` trait.
- Feature matrix preserved: `hxy-view` keeps `default = ["editor"]`, `serde`, and `dhat-bench` features; `default-features = false` (read-only viewer) must still compile.
- New crates use `version.workspace = true` (0.3.0 lockstep) and are registered in `[workspace.dependencies]`.
- Do not modify `crates/hxy-core`, the template-language crates, or any app behavior.

---

### Task 0: Create the jj workspace

**Files:** none (VCS only)

- [ ] **Step 1: Add a workspace and enter it**

```bash
cd /Users/lander/dev/hxy
jj workspace add --name gpui ../hxy-gpui
cd /Users/lander/dev/hxy-gpui
jj new main -m "wip: m0 extract hxy-editor"
```

- [ ] **Step 2: Verify baseline is green before touching anything**

Run: `cargo test --workspace --quiet 2>&1 | tail -20` and `cargo clippy --workspace --quiet 2>&1 | tail -5`
Expected: all tests pass, no clippy errors. Record the test count for later comparison.

All subsequent tasks run inside `/Users/lander/dev/hxy-gpui`.

---

### Task 1: Scaffold crates/hxy-editor

**Files:**
- Create: `crates/hxy-editor/Cargo.toml`
- Create: `crates/hxy-editor/src/lib.rs`
- Modify: `Cargo.toml` (workspace root: `members` + `[workspace.dependencies]`)

**Interfaces:**
- Produces: an empty `hxy-editor` crate other tasks fill in; workspace dep alias `hxy-editor = { path = "crates/hxy-editor", version = "0.3.0" }`.

- [ ] **Step 1: Write crates/hxy-editor/Cargo.toml**

```toml
[package]
name = "hxy-editor"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description = "Framework-agnostic hex editor model: edit state, undo/redo, vim-style input"

[dependencies]
hxy-core = { workspace = true }
serde = { workspace = true, optional = true }
suture = { workspace = true, optional = true }
thiserror = { workspace = true, optional = true }
tracing = { workspace = true, optional = true }
web-time = "1.1.0"

[features]
default = ["editor"]
# Full editing surface: patch overlay, undo/redo, keystroke input.
# Disabling leaves selection/navigation state only, for read-only
# viewers that don't want to pull in suture.
editor = ["dep:suture", "dep:thiserror", "dep:tracing"]
# Serialize/Deserialize on EditEntry and InputMode so consumers can
# round-trip editor state through their own storage layer.
serde = ["dep:serde", "editor", "suture?/serde"]
```

- [ ] **Step 2: Write a minimal src/lib.rs**

```rust
//! Framework-agnostic hex-editor model. UI layers (egui: hxy-view,
//! gpui: hxy-view-gpui) translate native events into [`InputEvent`]s
//! and render from [`HexEditor`] state.

#![forbid(unsafe_code)]

pub mod events;

pub use events::Effect;
pub use events::InputEvent;
pub use events::Key;
pub use events::Modifiers;
```

(`events` module is written in Task 2; create `src/events.rs` containing only `//! Input events.` for now so this compiles.)

- [ ] **Step 3: Register in the workspace root Cargo.toml**

Add `"crates/hxy-editor"` to `members` (after `"crates/hxy-i18n"`), and to `[workspace.dependencies]`:

```toml
hxy-editor = { path = "crates/hxy-editor", version = "0.3.0" }
```

Remove the four `pub use` lines from Step 2's lib.rs until Task 2 lands the types (leave only the module declaration commented out or omit it) OR create the stub `events.rs` first. Prefer the stub: an empty module keeps lib.rs as written.

- [ ] **Step 4: Verify it builds**

Run: `cargo check -p hxy-editor`
Expected: success (stub events module has no items yet; if the `pub use` lines error, the stub is missing -- create it).

- [ ] **Step 5: Commit**

```bash
jj commit -m "feat(hxy-editor): scaffold framework-agnostic editor crate"
```

---

### Task 2: Framework-neutral input event types

**Files:**
- Create: `crates/hxy-editor/src/events.rs` (replacing stub)

**Interfaces:**
- Produces (used by Tasks 3 and 4):
  - `pub struct Modifiers { pub shift: bool, pub command: bool, pub alt: bool }`
  - `pub enum Key { ArrowLeft, ArrowRight, ArrowUp, ArrowDown, Escape, Tab, Backspace, Digit(u8), Letter(char) }`
  - `pub enum InputEvent { Key { key: Key, modifiers: Modifiers }, Text(String) }`
  - `pub enum Effect { CopyText(String) }`
  - `pub enum Disposition { Consumed, Passed }`

- [ ] **Step 1: Write the failing test** (in `events.rs` `#[cfg(test)] mod tests`)

```rust
#[test]
fn hex_nibble_from_key() {
    assert_eq!(Key::Digit(0).hex_nibble(), Some(0));
    assert_eq!(Key::Digit(9).hex_nibble(), Some(9));
    assert_eq!(Key::Letter('a').hex_nibble(), Some(0xA));
    assert_eq!(Key::Letter('f').hex_nibble(), Some(0xF));
    assert_eq!(Key::Letter('g').hex_nibble(), None);
    assert_eq!(Key::Escape.hex_nibble(), None);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p hxy-editor hex_nibble -- --nocapture`
Expected: FAIL (types not defined).

- [ ] **Step 3: Implement events.rs**

```rust
//! Framework-neutral input events. UI adapters translate their
//! native key/text events into these; unmapped keys never reach the
//! editor and stay with the host UI.

/// Modifier state accompanying a key press. `command` is the
/// platform primary modifier (cmd on macOS, ctrl elsewhere), matching
/// the egui convention the editor logic was written against.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub command: bool,
    pub alt: bool,
}

/// Keys the editor reacts to. `Letter` carries the lowercase ASCII
/// letter ('a'..='z'); shift state travels in [`Modifiers`]. `Digit`
/// carries 0..=9. Anything not representable here is not translated
/// by adapters and therefore never consumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    Escape,
    Tab,
    Backspace,
    Digit(u8),
    Letter(char),
}

impl Key {
    /// Hex-digit value for nibble typing: 0-9 and a-f.
    pub fn hex_nibble(self) -> Option<u8> {
        match self {
            Key::Digit(d) if d <= 9 => Some(d),
            Key::Letter(c @ 'a'..='f') => Some(c as u8 - b'a' + 10),
            _ => None,
        }
    }
}

/// One input event fed to [`crate::InputFilter::feed`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputEvent {
    Key { key: Key, modifiers: Modifiers },
    Text(String),
}

/// Side effect requested by input application. The UI adapter
/// executes these with its native facilities.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Write text to the system clipboard (vim yank/delete).
    CopyText(String),
}

/// Whether the editor consumed an event. `Passed` events stay with
/// the host UI (e.g. remain in egui's queue for other widgets).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    Consumed,
    Passed,
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p hxy-editor`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
jj commit -m "feat(hxy-editor): neutral input event and effect types"
```

---

### Task 3: Copy the edit model + HexEditor + dispatchers into hxy-editor

This is the core move. COPY (do not yet delete) from `hxy-view`, replacing the egui event drain with the filter/apply API. `crates/hxy-view` is untouched in this task and keeps compiling.

**Files:**
- Create: `crates/hxy-editor/src/editor.rs` (from `crates/hxy-view/src/editor.rs`, verbatim)
- Create: `crates/hxy-editor/src/input.rs` (from `crates/hxy-view/src/input.rs`, egui drain replaced)
- Create: `crates/hxy-editor/src/vim.rs` (from `crates/hxy-view/src/vim.rs`, egui drain + clipboard replaced)
- Modify: `crates/hxy-editor/src/lib.rs` (HexEditor from `crates/hxy-view/src/lib.rs:99-663`, `Pane` from `lib.rs:779-787`, new filter/apply/view_parts API)

**Interfaces:**
- Consumes: Task 2 types.
- Produces (Task 4 relies on exactly these):
  - `pub struct HexEditor` with every existing public method EXCEPT `view()`, `on_response()`, `handle_input()` (those become egui-side ext-trait methods), plus:
  - `pub fn input_filter(&self) -> InputFilter`
  - `pub fn apply_input(&mut self, batch: InputBatch) -> Vec<Effect>`
  - `pub fn view_parts(&mut self) -> ViewParts<'_>`
  - `pub fn on_frame(&mut self, scroll_offset: f32, columns: ColumnCount, visible_range: Option<ByteRange>, interacted_pane: Option<Pane>)`
  - `pub struct InputFilter` with `pub fn feed(&mut self, event: &InputEvent) -> Disposition` and `pub fn finish(self) -> InputBatch`
  - `pub struct InputBatch` (opaque)
  - `pub struct ViewParts<'e> { pub source: &'e std::sync::Arc<dyn HexSource>, pub selection: &'e mut Option<Selection>, pub active_pane: Pane, pub nibble_high: Option<bool>, pub pending_scroll: Option<f32>, pub pending_scroll_to_byte: Option<ByteOffset> }`
  - `pub enum Pane { Hex, Ascii }` (moved)
  - Re-exports: `EditEntry`, `EditMode`, `TypingMode`, `WriteError`, `InputMode`, `VimMode`, `VimState`, `Pending`, `FindDir`, `RegisterOrigin`, `SCROLLOFF_ROWS`

**Porting rules (follow exactly):**

1. `editor.rs`: copy verbatim including its `#[cfg(test)]` module. Only doc-link paths change if they referenced `crate::HexEditor` items that stay put (they do not -- HexEditor also lands in this crate's lib.rs).
2. `Pane` moves into lib.rs of hxy-editor (editor logic matches on it). Keep the doc comment.
3. `HexEditor`: copy the struct and all methods from hxy-view lib.rs EXCEPT `view()`, `on_response()`, `handle_input()`. Add:
   - `view_parts()`: takes `self.pending_scroll.take()` and `self.pending_scroll_to_byte.take()`; `nibble_high` is `Some(self.edit.edit_high_nibble)` when the `editor` feature is on and `self.edit.mode == EditMode::Mutable`, else `None`.
   - `on_frame(...)`: sets `self.scroll_offset`, `self.last_columns = Some(columns)`, `self.last_visible_range = visible_range`, and calls `self.set_active_pane(p)` when `interacted_pane` is `Some(p)` -- byte-for-byte the body of the old `on_response`.
4. Dispatch split. The old `input::dispatch(editor, ctx)` and `vim::dispatch(editor, ctx)` each had three phases: (a) frame-begin bookkeeping, (b) an egui `retain` closure turning events into presses while deciding consumption, (c) applying presses + frame-end bookkeeping (scrolloff, `last_cursor_offset`). Phase (b) becomes `InputFilter::feed`; phases (a)+(c) become `apply_input`. Concretely:
   - `InputFilter` is constructed by `HexEditor::input_filter()` from snapshots: `input_mode`, `vim.mode`, `vim.pending`, `active_pane`, and (editor feature) `edit.mode == Mutable` / `edit.typing_mode == Insert`. It holds a `Vec<Press>` where `Press` is a private enum unifying the existing `EditPress` and `VimPress` (keep both enums, wrap: `enum Press { Edit(EditPress), Vim(VimPress) }`).
   - `feed` reproduces the retain-closure decision tables verbatim, with these mappings: `egui::Key::A..=F` -> `Key::Letter('a'..='f')` via `Key::hex_nibble`; vim letter arms (`H J K L W B E F T I R A V Y P D X G`) -> `Key::Letter(..)` lowercase with `modifiers.shift` replacing the old `shift` checks; `Num0..=Num9` -> `Key::Digit(..)`; `$` stays `Key::Digit(4)` with `shift`.
   - vim Insert/Replace Escape semantics: the old code ran `consume_key(NONE, Escape)` before the drain and, when Escape was present, returned without processing other events. Replicate: while the filter is in Insert/Replace mode, an Escape key (no modifiers) is Consumed and latches `exit_insert`; after the latch every further event is Passed. `apply_input` with `exit_insert` set only performs the mode pop (set `VimMode::Normal`, `set_typing_mode(Replace)`, `reset_edit_nibble`) and ignores any presses collected before the Escape.
   - vim Normal/Visual text-event rule is unchanged: `Text` events are always Consumed while in vim Normal/Visual (they resolve pending find-char/text-object or are swallowed); in Default mode `Text` is Consumed only when at least one char was accepted for ASCII-pane typing.
   - Key events with `command` or `alt` are always Passed (both dispatchers).
   - `apply_input` MUST run its frame-begin bookkeeping (external-cursor-move detection: `reset_edit_nibble` + `push_history_boundary` + `last_cursor_offset` update) even when the batch is empty, exactly as the old dispatchers ran once per frame before draining. Adapters therefore call `input_filter`/`apply_input` unconditionally every frame.
5. Clipboard: `stash_register(editor, ctx, bytes)` becomes `stash_register(editor, effects: &mut Vec<Effect>, bytes)`, pushing `Effect::CopyText(text)`. Thread `&mut Vec<Effect>` through `yank_selection`, `yank_rows`, `delete_selection`, `delete_rows`, `delete_byte_under_cursor` in place of `ctx`. `apply_input` owns the Vec and returns it.
6. Feature gates: every `#[cfg(feature = "editor")]` copies as-is (the feature exists on hxy-editor). `InputMode` keeps its `serde` derive gate.
7. Copy the `#[cfg(test)]` modules from editor.rs and vim.rs verbatim (vim tests construct `HexEditor` directly and call private fns -- they live in the same crate again, so they compile unchanged, except `ed.selection = ...` assignments: `selection` is a private field; the tests are in-crate so this still works).

- [ ] **Step 1: Write failing dispatch-level tests first** (`crates/hxy-editor/tests/dispatch.rs`)

```rust
use std::sync::Arc;

use hxy_core::ByteOffset;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_core::Selection;
use hxy_editor::Disposition;
use hxy_editor::Effect;
use hxy_editor::HexEditor;
use hxy_editor::InputEvent;
use hxy_editor::InputMode;
use hxy_editor::Key;
use hxy_editor::Modifiers;
use hxy_editor::Pane;
use hxy_editor::VimMode;

fn editor(bytes: &[u8]) -> HexEditor {
    let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes.to_vec()));
    let mut ed = HexEditor::new(source);
    ed.set_selection(Some(Selection::caret(ByteOffset::new(0))));
    ed
}

fn key(k: Key) -> InputEvent {
    InputEvent::Key { key: k, modifiers: Modifiers::default() }
}

fn shift(k: Key) -> InputEvent {
    InputEvent::Key { key: k, modifiers: Modifiers { shift: true, ..Modifiers::default() } }
}

fn feed_all(ed: &mut HexEditor, events: &[InputEvent]) -> Vec<Effect> {
    let mut filter = ed.input_filter();
    for e in events {
        filter.feed(e);
    }
    ed.apply_input(filter.finish())
}

#[test]
fn typing_two_hex_digits_writes_byte_and_advances() {
    let mut ed = editor(&[0x00, 0x11]);
    feed_all(&mut ed, &[key(Key::Letter('a')), key(Key::Letter('b'))]);
    let range = hxy_core::ByteRange::new(ByteOffset::new(0), ByteOffset::new(1)).unwrap();
    assert_eq!(ed.source().read(range).unwrap()[0], 0xAB);
    assert_eq!(ed.selection().unwrap().cursor.get(), 1);
}

#[test]
fn arrow_right_with_shift_extends_selection() {
    let mut ed = editor(&[0x00, 0x11, 0x22]);
    ed.set_active_pane(Pane::Ascii);
    feed_all(&mut ed, &[shift(Key::ArrowRight), shift(Key::ArrowRight)]);
    let sel = ed.selection().unwrap();
    assert_eq!(sel.anchor.get(), 0);
    assert_eq!(sel.cursor.get(), 2);
}

#[test]
fn ascii_pane_text_event_types_byte() {
    let mut ed = editor(&[0x00]);
    ed.set_active_pane(Pane::Ascii);
    feed_all(&mut ed, &[InputEvent::Text("Z".to_owned())]);
    let range = hxy_core::ByteRange::new(ByteOffset::new(0), ByteOffset::new(1)).unwrap();
    assert_eq!(ed.source().read(range).unwrap()[0], b'Z');
}

#[test]
fn command_modifier_passes_through() {
    let mut ed = editor(&[0x00]);
    let mut filter = ed.input_filter();
    let ev = InputEvent::Key {
        key: Key::Letter('a'),
        modifiers: Modifiers { command: true, ..Modifiers::default() },
    };
    assert_eq!(filter.feed(&ev), Disposition::Passed);
}

#[test]
fn vim_motion_with_count() {
    let mut ed = editor(&[0u8; 64]);
    ed.set_input_mode(InputMode::Vim);
    // 2j = down two rows (16 columns default) -> offset 32.
    feed_all(&mut ed, &[key(Key::Digit(2)), key(Key::Letter('j'))]);
    assert_eq!(ed.selection().unwrap().cursor.get(), 32);
}

#[test]
fn vim_yank_row_emits_copy_effect() {
    let mut ed = editor(b"0123456789abcdef");
    ed.set_input_mode(InputMode::Vim);
    let effects = feed_all(&mut ed, &[key(Key::Letter('y')), key(Key::Letter('y'))]);
    assert_eq!(effects.len(), 1);
    let Effect::CopyText(text) = &effects[0];
    assert!(text.starts_with("30 31 32"), "hex-formatted yank, got: {text}");
}

#[test]
fn vim_insert_escape_swallows_batch_and_pops_mode() {
    let mut ed = editor(&[0x00]);
    ed.set_input_mode(InputMode::Vim);
    feed_all(&mut ed, &[key(Key::Letter('i'))]);
    assert_eq!(ed.vim_state().mode, VimMode::Insert);
    // Escape plus a trailing 'a' in one batch: mode pops, 'a' is NOT typed.
    feed_all(&mut ed, &[key(Key::Escape), key(Key::Letter('a'))]);
    assert_eq!(ed.vim_state().mode, VimMode::Normal);
    let range = hxy_core::ByteRange::new(ByteOffset::new(0), ByteOffset::new(1)).unwrap();
    assert_eq!(ed.source().read(range).unwrap()[0], 0x00);
}

#[test]
fn empty_batch_still_runs_frame_bookkeeping() {
    let mut ed = editor(&[0x00, 0x11]);
    // Simulate an external cursor move (mouse click), then an empty
    // input frame: the nibble cursor must reset to high.
    feed_all(&mut ed, &[key(Key::Letter('a'))]); // half-typed byte, nibble now low
    ed.set_selection(Some(Selection::caret(ByteOffset::new(1))));
    feed_all(&mut ed, &[]);
    // Typing one digit now must hit the HIGH nibble of byte 1.
    feed_all(&mut ed, &[key(Key::Letter('c'))]);
    let range = hxy_core::ByteRange::new(ByteOffset::new(1), ByteOffset::new(2)).unwrap();
    assert_eq!(ed.source().read(range).unwrap()[0], 0xC1);
}
```

Note for the implementer: `set_selection` already resets the nibble cursor itself; the `empty_batch` test verifies the apply-side path does not corrupt that. If the old `dispatch` behavior differs in a detail (verify against `crates/hxy-view/src/input.rs:105-131`), match the OLD behavior and adjust the test -- behavior parity with the egui app outranks this test's guess.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p hxy-editor --test dispatch`
Expected: FAIL to compile (API absent).

- [ ] **Step 3: Perform the copy per the porting rules above**

Order that keeps `cargo check -p hxy-editor` convergent: editor.rs first, then lib.rs (HexEditor + Pane + ViewParts + re-exports), then input.rs, then vim.rs, then InputFilter/InputBatch/apply_input in lib.rs (or a new `dispatch.rs` module if lib.rs grows past ~700 lines; prefer `dispatch.rs`).

- [ ] **Step 4: Run the full crate test suite**

Run: `cargo test -p hxy-editor`
Expected: PASS -- moved editor.rs unit tests, moved vim.rs unit tests, Task 2 events tests, and the new dispatch tests.

Also run: `cargo check -p hxy-editor --no-default-features`
Expected: success (read-only configuration).

Also run: `cargo check -p hxy-editor --features serde`
Expected: success.

- [ ] **Step 5: Clippy**

Run: `cargo clippy -p hxy-editor --all-features`
Expected: no warnings introduced by this crate.

- [ ] **Step 6: Commit**

```bash
jj commit -m "feat(hxy-editor): port edit model, HexEditor, and vim dispatch from hxy-view"
```

---

### Task 4: Refactor hxy-view onto hxy-editor

Delete the copied modules from `hxy-view`, depend on `hxy-editor`, add the egui adapter. `crates/hxy` still compiles against `hxy-view` re-exports (it is fixed up in Task 5 only if trait imports are needed).

**Files:**
- Delete: `crates/hxy-view/src/editor.rs`, `crates/hxy-view/src/vim.rs`
- Rewrite: `crates/hxy-view/src/input.rs` (becomes the egui adapter)
- Modify: `crates/hxy-view/src/lib.rs` (remove HexEditor/Pane, add ext trait + re-exports)
- Modify: `crates/hxy-view/Cargo.toml`

**Interfaces:**
- Consumes: Task 3 API.
- Produces (crates/hxy relies on these):
  - `pub use hxy_editor::{HexEditor, EditEntry, EditMode, TypingMode, WriteError, InputMode, VimMode, VimState, Pane, Effect, InputEvent, Key, Modifiers};` from `hxy_view`
  - `pub trait HexEditorExt { fn view(&mut self) -> HexView<'_, dyn HexSource>; fn handle_input(&mut self, ctx: &egui::Context); fn on_response(&mut self, response: &HexViewResponse, columns: ColumnCount); }` implemented for `HexEditor`
  - `NibbleSide`, `HexView`, `HexViewResponse`, and all render types unchanged.

- [ ] **Step 1: Cargo.toml changes**

In `[dependencies]` add `hxy-editor = { workspace = true, default-features = false }`. Rewrite features:

```toml
[features]
default = ["editor"]
editor = ["hxy-editor/editor"]
serde = ["dep:serde", "editor", "hxy-editor/serde"]
```

Keep `suture`, `thiserror`, `tracing` deps ONLY if still referenced after the deletion (expected: they are not; remove them and the `suture?/serde` fragment). Keep `dhat-bench` as-is.

- [ ] **Step 2: Delete moved code, add re-exports and ext trait**

In lib.rs: delete the `HexEditor` struct/impl (old lines 99-663), the `Pane` enum, `mod editor`, `mod vim`, and the old `pub use editor::*` / `pub use vim::*` lines. Add:

```rust
pub use hxy_editor::Effect;
pub use hxy_editor::HexEditor;
pub use hxy_editor::InputEvent;
pub use hxy_editor::InputMode;
pub use hxy_editor::Key;
pub use hxy_editor::Modifiers;
pub use hxy_editor::Pane;
pub use hxy_editor::VimMode;
pub use hxy_editor::VimState;
#[cfg(feature = "editor")]
pub use hxy_editor::{EditEntry, EditMode, TypingMode, WriteError};

/// Egui-side conveniences for [`HexEditor`]: build the per-frame
/// [`HexView`], drain egui input, and latch the frame response.
/// Import this trait to keep the pre-split call sites
/// (`editor.view()`, `editor.handle_input(ctx)`,
/// `editor.on_response(..)`) compiling unchanged.
pub trait HexEditorExt {
    fn view(&mut self) -> HexView<'_, dyn HexSource>;
    fn handle_input(&mut self, ctx: &egui::Context);
    fn on_response(&mut self, response: &HexViewResponse, columns: ColumnCount);
}

impl HexEditorExt for HexEditor {
    fn view(&mut self) -> HexView<'_, dyn HexSource> {
        let parts = self.view_parts();
        let nibble = parts.nibble_high.map(|h| if h { NibbleSide::High } else { NibbleSide::Low });
        let mut view = HexView::new(parts.source.as_ref(), parts.selection)
            .active_pane(Some(parts.active_pane))
            .nibble_cursor(nibble);
        if let Some(s) = parts.pending_scroll {
            view = view.scroll_to(s);
        }
        if let Some(b) = parts.pending_scroll_to_byte {
            view = view.scroll_to_byte(b);
        }
        view
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        input::handle_input(self, ctx);
    }

    fn on_response(&mut self, response: &HexViewResponse, columns: ColumnCount) {
        self.on_frame(response.scroll_offset, columns, response.visible_range, response.interacted_pane);
    }
}
```

- [ ] **Step 3: Rewrite input.rs as the egui adapter**

```rust
//! Egui adapter: translates egui events into hxy-editor's neutral
//! [`InputEvent`]s, feeds them through the editor's filter (which
//! decides consumption), and executes returned effects.

use hxy_editor::Disposition;
use hxy_editor::Effect;
use hxy_editor::HexEditor;
use hxy_editor::InputEvent;
use hxy_editor::Key;
use hxy_editor::Modifiers;

fn translate_key(key: egui::Key) -> Option<Key> {
    use egui::Key as K;
    Some(match key {
        K::ArrowLeft => Key::ArrowLeft,
        K::ArrowRight => Key::ArrowRight,
        K::ArrowUp => Key::ArrowUp,
        K::ArrowDown => Key::ArrowDown,
        K::Escape => Key::Escape,
        K::Tab => Key::Tab,
        K::Backspace => Key::Backspace,
        K::Num0 => Key::Digit(0),
        K::Num1 => Key::Digit(1),
        K::Num2 => Key::Digit(2),
        K::Num3 => Key::Digit(3),
        K::Num4 => Key::Digit(4),
        K::Num5 => Key::Digit(5),
        K::Num6 => Key::Digit(6),
        K::Num7 => Key::Digit(7),
        K::Num8 => Key::Digit(8),
        K::Num9 => Key::Digit(9),
        K::A => Key::Letter('a'),
        K::B => Key::Letter('b'),
        // ... continue for every letter C..=Z -> 'c'..='z' ...
        K::Z => Key::Letter('z'),
        _ => return None,
    })
}

fn translate_modifiers(m: egui::Modifiers) -> Modifiers {
    Modifiers { shift: m.shift, command: m.command, alt: m.alt }
}

pub(crate) fn handle_input(editor: &mut HexEditor, ctx: &egui::Context) {
    if ctx.egui_wants_keyboard_input() {
        return;
    }
    let mut filter = editor.input_filter();
    ctx.input_mut(|i| {
        i.events.retain(|event| {
            let translated = match event {
                egui::Event::Key { key, pressed: true, modifiers, .. } => translate_key(*key)
                    .map(|k| InputEvent::Key { key: k, modifiers: translate_modifiers(*modifiers) }),
                egui::Event::Text(s) => Some(InputEvent::Text(s.clone())),
                _ => None,
            };
            match translated {
                Some(ev) => filter.feed(&ev) == Disposition::Passed,
                None => true,
            }
        });
    });
    for effect in editor.apply_input(filter.finish()) {
        match effect {
            Effect::CopyText(text) => ctx.copy_text(text),
        }
    }
}
```

Write the full A-Z arm list; no `..=` range patterns exist for egui::Key.

IMPORTANT parity check: the old dispatchers ran even when `i.events` was empty (frame bookkeeping); the adapter above preserves that because `apply_input` is unconditional. Do NOT add an early return on empty events.

- [ ] **Step 4: Fix stragglers inside hxy-view**

`bin/hexview_dhat.rs`, `tests/interaction.rs`, and any lib.rs internals referencing `editor.` fields or `input::`/`vim::` items: update imports to `hxy_editor::` types and add `use hxy_view::HexEditorExt;` (tests) or `use crate::HexEditorExt;` (bin). Grep to enumerate:

```bash
grep -rn "vim::\|editor::\|handle_input\|\.view()\|on_response" crates/hxy-view/src crates/hxy-view/tests crates/hxy-view/bin
```

- [ ] **Step 5: Run the hxy-view suite (unit + kittest interaction)**

Run: `cargo test -p hxy-view` and `cargo test -p hxy-view --no-default-features` and `cargo check -p hxy-view --features serde`
Expected: PASS with the same test count as the Task 0 baseline for this crate. The kittest interaction tests exercise the full egui event path through the new adapter -- they are the M0 behavioral gate for typing, vim, selection, and scrolling.

- [ ] **Step 6: Commit**

```bash
jj commit -m "refactor(hxy-view): drive rendering from hxy-editor via egui adapter"
```

---

### Task 5: Fix crates/hxy call sites and verify the app

**Files:**
- Modify: any `crates/hxy/src` file calling `.view()`, `.handle_input(`, or `.on_response(` on a `HexEditor` (grep list; expected: `src/view/hex_body.rs`, `src/compare/pane.rs`, `src/compare/mod.rs`, `src/app/shortcuts.rs`, `src/app/wasm.rs`, `src/files/mod.rs`)

**Interfaces:**
- Consumes: `hxy_view::HexEditorExt` trait and unchanged re-exports.

- [ ] **Step 1: Enumerate and fix**

```bash
grep -rln "\.view()\|handle_input\|on_response" crates/hxy/src
```

Add `use hxy_view::HexEditorExt;` to each hit. No other changes expected; if a call site used a type that stopped being re-exported, re-export it from hxy-view rather than changing the app import.

- [ ] **Step 2: Full workspace verification**

Run: `cargo test --workspace` and `cargo clippy --workspace --all-targets`
Expected: identical pass count to the Task 0 baseline (plus the new hxy-editor tests); zero clippy warnings.

- [ ] **Step 3: Behavior smoke test of the real app**

Run: `cargo run -p hxy -- README.md` (or any file), then manually verify: hex typing overwrites nibbles and advances; arrows + shift-select work; vim mode (toggle via its setting/palette) hjkl/visual/yank works and yank reaches the system clipboard; undo/redo; minimap scroll. Close the app.
Expected: behavior indistinguishable from `main`.

- [ ] **Step 4: Commit**

```bash
jj commit -m "refactor(hxy): import HexEditorExt for split hex editor API"
```

---

### Task 6: Extract palette-core from egui-palette

**Files:**
- Create: `crates/palette-core/Cargo.toml`, `crates/palette-core/src/lib.rs`, `crates/palette-core/src/fuzzy.rs`
- Modify: `crates/egui-palette/Cargo.toml`, `crates/egui-palette/src/lib.rs`
- Delete: `crates/egui-palette/src/fuzzy.rs`
- Modify: root `Cargo.toml` (member + workspace dep `palette-core = { path = "crates/palette-core", version = "0.3.0" }`)

**Interfaces:**
- Produces (`palette-core`, no egui dependency; `publish = false` until named for crates.io):
  - `pub struct Entry<A> { pub title: String, pub subtitle: Option<String>, pub icon: Option<String>, pub shortcut: Option<String>, pub disabled: bool, pub data: A }` + its builder methods (moved verbatim from `crates/egui-palette/src/lib.rs:123-168`)
  - `pub struct State { ... }` + `open()` / `close()` (moved verbatim from lib.rs; all fields are plain types)
  - `pub enum Outcome<A> { Picked(A), Dismissed(DismissReason<K>) }` -- genericize: `pub enum Outcome<A, K> { Picked(A), Dismissed(DismissReason<K>) }`
  - `pub enum DismissReason<K> { Key(K), Backdrop }`
  - `pub struct MatchResult { pub index: usize, pub match_indices: Vec<u32> }` and `pub fn filter_and_sort<A, F>(...) -> Vec<MatchResult>` (fuzzy.rs moved verbatim)
  - Re-exports: `pub use nucleo_matcher::pattern::CaseMatching; pub use nucleo_matcher::pattern::Normalization; pub use nucleo_matcher::Config as MatcherConfig;` -- copy however egui-palette's lib.rs currently names these (check its `use`/`pub use` lines and keep identical aliasing).
- `egui-palette` keeps API compatibility: `pub use palette_core::{Entry, State, MatchResult, filter_and_sort, ...};`, `pub type Outcome<A> = palette_core::Outcome<A, egui::Key>;`, `pub type DismissReason = palette_core::DismissReason<egui::Key>;`. If `type` aliases break pattern-matching call sites in crates/hxy (enum variants cannot be constructed through a type alias in all positions), instead re-export the generic enums directly and fix the handful of app call sites -- prefer whichever keeps `crates/hxy` diffs smallest.

- [ ] **Step 1: Scaffold palette-core Cargo.toml**

```toml
[package]
name = "palette-core"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description = "Framework-agnostic command palette state and fuzzy filtering"
publish = false

[dependencies]
nucleo-matcher = "0.3"
```

(Match the nucleo-matcher version already in egui-palette's Cargo.toml -- check and pin identically.)

- [ ] **Step 2: Move code**

Move `fuzzy.rs` verbatim (tests included). Move `Entry`, `State`, `Outcome`/`DismissReason` (genericized), and the nucleo config re-exports into palette-core's lib.rs. Leave `Style`, `Anchor`, `show()`, and all rendering in egui-palette; update its imports to `use palette_core::...`.

- [ ] **Step 3: Verify**

Run: `cargo test -p palette-core -p egui-palette` then `cargo test --workspace` and `cargo clippy --workspace --all-targets`
Expected: fuzzy tests pass in their new home; workspace green; app palette behavior unchanged (open the command palette in a `cargo run -p hxy` smoke test: fuzzy filtering, arrow selection, Enter, Escape).

- [ ] **Step 4: Commit**

```bash
jj commit -m "refactor(egui-palette): extract framework-agnostic palette-core"
```

---

### Task 7: Milestone gate: adversarial review and cleanup

**Files:** review-driven; no planned edits.

- [ ] **Step 1: Full verification sweep**

Run: `cargo test --workspace`, `cargo clippy --workspace --all-targets`, `cargo check -p hxy-view --no-default-features`, `cargo check -p hxy-editor --no-default-features`
Expected: all green.

- [ ] **Step 2: Adversarial review**

Dispatch a FRESH subagent to review the entire M0 diff (`jj diff -r 'main..@'`) against:
1. The global CLAUDE.md rules (typed errors, newtypes, no unwrap_or on fallible data, ASCII, comment discipline, no scope creep).
2. Behavior parity: every branch of the old egui retain closures must have a corresponding branch in `InputFilter::feed` (have the reviewer diff old `crates/hxy-view/src/input.rs`/`vim.rs` at `main` against the new `crates/hxy-editor/src` versions decision-by-decision).
3. API surface: no accidental publicization (e.g. EditState internals) beyond what Tasks 3/4/6 specify.

- [ ] **Step 3: Fix findings, re-run verification, commit fixes**

Each fix is its own focused commit (`fix(hxy-editor): ...`).

- [ ] **Step 4: Squash/tidy the wip description**

```bash
jj log -r 'main..@'
```

Ensure the change series reads as the 6 planned commits plus fixes; give any remaining `wip:` change a real description or abandon it if empty.

---

## Self-review notes

- Spec coverage: M0 = hxy-editor extraction + palette logic extraction; both covered (Tasks 1-5, 6). GPUI crates intentionally absent (M1+).
- The `empty_batch_still_runs_frame_bookkeeping` test encodes a behavior guess; Task 3 Step 1 explicitly instructs verifying against the old source and preferring parity.
- Type consistency: `Disposition` returned by `feed`; `apply_input` returns `Vec<Effect>`; ext trait signatures match Task 3's produces-block.
