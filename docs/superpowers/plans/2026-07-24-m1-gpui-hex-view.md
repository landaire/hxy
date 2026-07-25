# M1: GPUI Hex View and Shell Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A GPUI hex-view widget (`hxy-view-gpui`) driven by the shared `hxy-editor` model, inside a minimal `hxy-gpui` shell app: open a file, render hex/ASCII grids with address gutter and minimap, full keyboard nav including vim, selection and drag-select, in-memory editing.

**Architecture:** `HexPane` is a retained GPUI entity owning a `hxy_editor::HexEditor`. Rendering is a `canvas()` element painted with `shape_line`/`paint_quad` per row (Zed terminal model), with pure-math layout in a separate geometry module. Native key/mouse events translate into `hxy_editor::InputEvent`s through the same `input_filter`/`apply_input` contract the egui adapter uses, so editor behavior is identical by construction.

**Tech Stack:** gpui + gpui_platform (git, pinned zed rev), gpui-component (git, pinned rev), hxy-editor, hxy-core. Reference: `docs/superpowers/plans/2026-07-24-m1-gpui-api-notes.md` (cited API facts; committed in Task 1).

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-24-gpui-port-design.md`. M1 scope: "hxy-view-gpui plus a minimal shell (open file, hex/ASCII grid, keyboard nav including vim, selection, drag-select, editing, minimap)". No save-to-disk, no docking, no panels (M2+). No features beyond this list.
- Dependency pins (crates.io route; the git route is unusable -- gpui-component's manifest pulls gpui from zed tip unpinned, unpatchable, and zed tip needs Rust > 1.92):
  - `gpui = "=0.2.2"` and `gpui-component = "=0.5.1"` from crates.io. No gpui_platform crate on this route.
  - Bootstrap is `gpui::Application::new()` (see api-notes "Alternative crates.io route"; rusthex is the reference consumer).
  - API-notes caveat: sections researched at git HEAD may differ at 0.2.2/0.5.1. Where a name differs, the published source/docs.rs win; append corrections to the api-notes file under a "crates.io corrections" heading as they are discovered.
- NESTED WORKSPACE (user decision after a Task 1 blocker): the GPUI crates live in `gpui/` -- a separate Cargo workspace with its own `Cargo.toml` and `Cargo.lock` -- because the zed git tree cannot co-resolve with the egui stack in one lockfile (wgpu minor pins, `core-foundation =0.10.0` exact pin, accesskit conflicts). Members: `gpui/hxy-gpui`, `gpui/hxy-view-gpui`. Shared crates are path deps (`hxy-editor = { path = "../crates/hxy-editor" }` etc.). The ROOT workspace Cargo.toml gets NO gpui entries and no new members. Every "workspace" verification command in this plan means: run it in BOTH workspaces (`cargo test --workspace` at the root, and `cd gpui && cargo test --workspace`). The nested workspace needs its own `[profile.dev.package."*"] opt-level = 2`.
- jj only, never git; non-interactive `-m` messages; conventional commits; NO Co-Authored-By or AI attribution; ASCII only in code/comments/commit messages; no separator comments; no historical framing.
- Typed thiserror errors; newtypes/enums over bools and loose primitives; scrutinize new `unwrap_or*` on fallible data.
- i18n: user-visible strings in `hxy-gpui` go through `hxy_i18n::t` / `t_args` (widget crate `hxy-view-gpui` has no user-visible prose, only glyphs/addresses, and must NOT depend on hxy-i18n).
- Theming: idiomatic gpui-component; follow system dark/light at startup and on runtime change.
- The GPUI crates are desktop-only; macOS is the verify target. Do not gate on Linux/Windows.
- Work continues in the jj workspace `/Users/lander/dev/hxy-gpui`, stacked on the M0 head (change `mssqryyvssxp`, commit `4712179`).
- dev profile: the workspace already has `[profile.dev.package."*"] opt-level = 2`, which covers gpui's paint hot path in dev builds.
- Every task ends: tests green, `cargo clippy -p <touched crates> --all-targets` zero warnings, commit.
- GUI cannot be launched headlessly here: interactive verification uses `#[gpui::test]` (`TestAppContext`, `VisualTestContext::simulate_keystrokes/simulate_event`); a human smoke test happens at the milestone gate.

---

### Task 1: Pinned deps and the hxy-gpui shell skeleton

**Files:**
- Create: `gpui/Cargo.toml` (NEW nested workspace root; the repo root Cargo.toml is NOT touched)
- Create: `gpui/hxy-gpui/Cargo.toml`
- Create: `gpui/hxy-gpui/src/main.rs`
- Commit also: `docs/superpowers/plans/2026-07-24-m1-gpui-api-notes.md` (already on disk, uncommitted)

**Interfaces:**
- Produces: a runnable `hxy-gpui` binary opening one themed window with a placeholder label; nested-workspace dep aliases `gpui`, `gpui_platform`, `gpui-component`, and path deps to the shared crates.

- [ ] **Step 1: Nested workspace root gpui/Cargo.toml**

```toml
[workspace]
resolver = "3"
members = ["hxy-gpui"]

[workspace.package]
version = "0.3.0"
edition = "2024"
rust-version = "1.92"
license = "MIT OR Apache-2.0"
authors = ["Lander Brandt"]
repository = "https://github.com/landaire/hxy"

[workspace.dependencies]
gpui = "=0.2.2"
gpui-component = "=0.5.1"
hxy-core = { path = "../crates/hxy-core", version = "0.3.0" }
hxy-editor = { path = "../crates/hxy-editor", version = "0.3.0" }
hxy-i18n = { path = "../crates/hxy-i18n", version = "0.3.0" }
tracing = "0.1.44"

[profile.dev.package."*"]
opt-level = 2
```

The ROOT Cargo.toml gets exactly one change: `exclude = ["gpui"]` under `[workspace]`, so cargo never treats the nested crates as stray members of the root workspace.

- [ ] **Step 2: gpui/hxy-gpui/Cargo.toml**

```toml
[package]
name = "hxy-gpui"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description = "GPUI port of the hxy hex editor"
publish = false

[dependencies]
gpui = { workspace = true }
gpui-component = { workspace = true }
hxy-core = { workspace = true }
hxy-editor = { workspace = true, features = ["editor"] }
hxy-i18n = { workspace = true }
tracing = { workspace = true }
```

- [ ] **Step 3: main.rs skeleton**

Follow the api-notes "App bootstrap" section (gpui-component examples pattern). Shape:

```rust
use gpui::prelude::*;
use gpui::App;
use gpui::Bounds;
use gpui::Window;
use gpui::WindowBounds;
use gpui::WindowOptions;
use gpui::px;
use gpui::size;

struct Workspace;

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::div()
            .size_full()
            .bg(gpui::rgb(0x000000))
            .child(hxy_i18n::t("gpui-shell-no-file"))
    }
}

fn main() {
    let app = gpui::Application::new();
    app.run(|cx: &mut App| {
        gpui_component::init(cx);
        let bounds = Bounds::centered(None, size(px(1024.0), px(768.0)), cx);
        cx.open_window(
            WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() },
            |window, cx| {
                gpui_component::Theme::sync_system_appearance(Some(window), cx);
                cx.new(|_| Workspace)
            },
        )
        .expect("open window");
        cx.activate(true);
    });
}
```

Adjust names against the api-notes file and the pinned sources if a signature differs (the notes are authoritative; they cite the pinned revs). Replace the hardcoded `bg` with the theme background once `cx.theme()` is available in scope (`use gpui_component::ActiveTheme;` -- see notes). Add the `gpui-shell-no-file` message ("No file open. Pass a path on the command line.") to hxy-i18n's Fluent resources following that crate's existing pattern for adding a key (look at `crates/hxy-i18n` and its .ftl files; copy the existing style, English locale at minimum, and mirror the key into the other locale files present with the same English text if no translation is available).

- [ ] **Step 4: Build and verify**

Run: `cd gpui && cargo build -p hxy-gpui` (first build compiles the zed tree; allow 10+ minutes) and `cd gpui && cargo clippy -p hxy-gpui --all-targets`.
Expected: clean build, zero warnings. Do NOT attempt `cargo run` (headless environment).

Also run: `cargo test --workspace --quiet` to confirm nothing else broke (Cargo.lock grew).

- [ ] **Step 5: Commit**

```bash
jj commit -m "feat(hxy-gpui): shell skeleton on pinned gpui/gpui-component" Cargo.toml gpui docs/superpowers/plans/2026-07-24-m1-gpui-api-notes.md docs/superpowers/plans/2026-07-24-m1-gpui-hex-view.md
```

(If `jj commit` with path arguments rejects the docs paths, commit everything in the working copy -- it is all this task's output.)

---

### Task 2: hxy-view-gpui crate and grid geometry

**Files:**
- Create: `gpui/hxy-view-gpui/Cargo.toml`
- Create: `gpui/hxy-view-gpui/src/lib.rs`
- Create: `gpui/hxy-view-gpui/src/geometry.rs`
- Modify: root `Cargo.toml` (member + `hxy-view-gpui = { path = "gpui/hxy-view-gpui", version = "0.3.0" }`)

**Interfaces:**
- Consumes: `hxy_core::{ByteOffset, ByteLen, ColumnCount}`, `hxy_editor::{Pane, NibbleCursor}`, `gpui::{Pixels, Point, Bounds, px}`.
- Produces (Tasks 3-6 rely on these exact names):

```rust
/// Font-derived cell dimensions, computed once per paint from the
/// window text system.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellMetrics {
    /// Advance width of one monospace character.
    pub char_w: Pixels,
    pub line_h: Pixels,
}

/// Horizontal layout of one row: x origins (relative to the grid's
/// content origin) for the address gutter and each hex / ascii cell.
/// All math in character units times char_w, mirroring the egui
/// RowLayout proportions (hxy-view/src/lib.rs, RowLayout::compute):
/// address gutter = address_chars + 2 chars gap; each hex cell is
/// 3 chars wide (2 glyphs + 1 space); 2 chars gap before the ascii
/// pane; ascii cells 1 char wide.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridGeometry {
    pub metrics: CellMetrics,
    pub columns: u16,
    pub address_chars: usize,
}

impl GridGeometry {
    pub fn new(metrics: CellMetrics, columns: ColumnCount, source_len: ByteLen) -> Self;
    /// Minimum hex digits to address the source, min 8 (same rule as
    /// egui's address_hex_width).
    pub fn address_chars_for(source_len: ByteLen) -> usize;
    pub fn address_x(&self) -> Pixels;           // 0
    pub fn hex_x(&self, col: u16) -> Pixels;     // gutter + col * 3 chars
    pub fn ascii_x(&self, col: u16) -> Pixels;
    pub fn row_width(&self) -> Pixels;           // total content width
    pub fn hex_cell_w(&self) -> Pixels;          // 2 chars (glyph pair, excl. spacing)
    /// Total rows incl. the EOF-cursor row (same rule as egui's
    /// row_count: empty source renders 1 row; len == k*cols renders
    /// k+1 rows).
    pub fn row_count(&self, source_len: ByteLen) -> u64;
    /// Hit-test a point (relative to content origin, y already
    /// adjusted for scroll) to a pane/byte/nibble.
    pub fn hit_test(&self, pos: Point<Pixels>, source_len: ByteLen) -> Option<GridHit>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridHit {
    pub pane: Pane,
    pub offset: ByteOffset,     // clamped to [0, source_len]
    pub nibble: NibbleCursor,   // always High in the ascii pane
}
```

- [ ] **Step 1: Crate scaffold**

Cargo.toml mirrors hxy-view-gpui's role (publish = false for now):

```toml
[package]
name = "hxy-view-gpui"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description = "GPUI hex-view widget driven by hxy-editor"
publish = false

[dependencies]
gpui = { workspace = true }
hxy-core = { workspace = true }
hxy-editor = { workspace = true, features = ["editor"] }
tracing = { workspace = true }
```

lib.rs starts as `#![forbid(unsafe_code)]`, `mod geometry;`, `pub use geometry::*;`.

- [ ] **Step 2: Write failing geometry tests** (in `geometry.rs` tests module; pure math, plain `#[test]`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use hxy_core::ByteLen;
    use hxy_core::ColumnCount;
    use gpui::point;
    use gpui::px;

    fn geo() -> GridGeometry {
        let metrics = CellMetrics { char_w: px(8.0), line_h: px(16.0) };
        GridGeometry::new(metrics, ColumnCount::new(16).unwrap(), ByteLen::new(256))
    }

    #[test]
    fn address_width_matches_egui_rule() {
        assert_eq!(GridGeometry::address_chars_for(ByteLen::new(0)), 8);
        assert_eq!(GridGeometry::address_chars_for(ByteLen::new(1u64 << 32)), 8);
        assert_eq!(GridGeometry::address_chars_for(ByteLen::new((1u64 << 32) + 1)), 9);
    }

    #[test]
    fn row_count_reserves_eof_row() {
        let g = geo();
        assert_eq!(g.row_count(ByteLen::new(0)), 1);
        assert_eq!(g.row_count(ByteLen::new(15)), 1);
        assert_eq!(g.row_count(ByteLen::new(16)), 2);
    }

    #[test]
    fn hex_cells_are_three_chars_apart() {
        let g = geo();
        assert_eq!(g.hex_x(1) - g.hex_x(0), px(24.0));
        assert_eq!(g.hex_cell_w(), px(16.0));
    }

    #[test]
    fn hit_test_left_half_of_hex_cell_is_high_nibble() {
        let g = geo();
        let x = g.hex_x(2) + px(3.0);
        let hit = g.hit_test(point(x, px(20.0)), ByteLen::new(256)).unwrap();
        assert_eq!(hit.pane, hxy_editor::Pane::Hex);
        assert_eq!(hit.offset.get(), 16 + 2);
        assert_eq!(hit.nibble, hxy_editor::NibbleCursor::High);
    }

    #[test]
    fn hit_test_ascii_pane() {
        let g = geo();
        let x = g.ascii_x(5) + px(1.0);
        let hit = g.hit_test(point(x, px(0.0)), ByteLen::new(256)).unwrap();
        assert_eq!(hit.pane, hxy_editor::Pane::Ascii);
        assert_eq!(hit.offset.get(), 5);
    }

    #[test]
    fn hit_test_clamps_past_last_column() {
        let g = geo();
        let x = g.ascii_x(15) + px(100.0);
        let hit = g.hit_test(point(x, px(0.0)), ByteLen::new(256));
        assert!(hit.is_none() || hit.unwrap().offset.get() <= 256);
    }
}
```

Fix the last test's assertion while implementing: decide (and encode in the test) the documented rule -- points right of the ascii pane return `None`; points in the gap between panes return `None`; y below the last row clamps to the last row. Match egui hxy-view's hovered_byte behavior where visible (it treats gaps as no-hit).

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p hxy-view-gpui`
Expected: FAIL to compile (types absent).

- [ ] **Step 4: Implement geometry.rs**

All positions are `char_w` multiples: gutter `address_chars + 2`; hex pane starts there; hex cell stride 3 chars; ascii pane at gutter + columns*3 + 2. `hit_test`: derive row from `pos.y / line_h` (clamp row to `row_count - 1`), column from x within the pane spans; nibble = left half vs right half of the 2-char glyph pair; offset = row * columns + col clamped to source_len (EOF caret position allowed at exactly source_len on the EOF row). Document the EOF rule on `hit_test`.

- [ ] **Step 5: Run tests, clippy; commit**

Run: `cargo test -p hxy-view-gpui && cargo clippy -p hxy-view-gpui --all-targets`
Expected: PASS, zero warnings.

```bash
jj commit -m "feat(hxy-view-gpui): grid geometry with egui-parity layout rules"
```

---

### Task 3: HexPane entity and canvas grid painting

**Files:**
- Create: `gpui/hxy-view-gpui/src/pane.rs`
- Create: `gpui/hxy-view-gpui/src/paint.rs`
- Modify: `gpui/hxy-view-gpui/src/lib.rs` (`mod pane; mod paint; pub use pane::HexPane;`)
- Modify: `gpui/hxy-gpui/src/main.rs` (embed a HexPane over a demo MemorySource so the shell shows a grid)

**Interfaces:**
- Consumes: Task 2 geometry; api-notes sections "canvas", "shape_line/TextRun", "paint_quad", "scroll", "theme".
- Produces:

```rust
pub struct HexPane {
    editor: hxy_editor::HexEditor,
    focus_handle: gpui::FocusHandle,
    columns: hxy_core::ColumnCount,
    /// Vertical scroll in fractional rows (row 0 at top when 0.0).
    scroll_rows: f32,
    /// Set during paint, consumed by input handlers (hit testing,
    /// page-size scrolling).
    last_frame: Option<FrameInfo>,
}

#[derive(Clone, Copy, Debug)]
pub struct FrameInfo {
    pub geometry: GridGeometry,
    pub content_origin: gpui::Point<gpui::Pixels>,
    pub rows_visible: f32,
    pub first_visible_row: u64,
}

impl HexPane {
    pub fn new(source: std::sync::Arc<dyn hxy_core::HexSource>, cx: &mut gpui::Context<Self>) -> Self;
    pub fn editor(&self) -> &hxy_editor::HexEditor;
    pub fn editor_mut(&mut self) -> &mut hxy_editor::HexEditor;  // callers must cx.notify()
    pub fn set_source(&mut self, source: std::sync::Arc<dyn hxy_core::HexSource>, cx: &mut gpui::Context<Self>);
}
impl gpui::Focusable for HexPane { /* focus_handle() */ }
impl gpui::Render for HexPane { /* div().track_focus(..).size_full().child(canvas(..)) */ }
```

- [ ] **Step 1: Render skeleton + scroll**

`render()` returns a `div()` that: `.track_focus(&self.focus_handle)`, fills its bounds, uses theme background (`cx.theme().background` per api-notes), attaches `.on_scroll_wheel(cx.listener(..))` adjusting `self.scroll_rows` (delta per api-notes: `ScrollDelta::Pixels` divides by line height; `ScrollDelta::Lines` adds rows directly; clamp to `[0, row_count - 1]`), calls `cx.notify()`, and contains the canvas element built in `paint.rs`.

- [ ] **Step 2: paint.rs**

One public function used by `render`: builds the `canvas(prepaint, paint)` element. In paint, per api-notes:

1. Resolve `CellMetrics` from `window.text_system()` (`em_advance` of the buffer font at the theme's mono font + `window.line_height()`; gpui-component theme exposes a mono font family -- use it, falling back to `font("Menlo")` style resolution per the notes if the theme token does not exist at the pinned rev).
2. Compute `GridGeometry`, visible row range from bounds height and `scroll_rows`, and store `FrameInfo` back on the entity (canvas closures receive the entity via the captured `cx.entity()` handle -- follow the notes' canvas pattern; the prepaint/paint closures are rebuilt per render so capturing is safe).
3. For each visible row: read bytes via `editor.source().read(range)` (handle `Err` by painting nothing for the row and `tracing::warn!` once per paint, not per row); format the address (uppercase zero-padded hex, `address_chars` wide); build ONE string per row per region (address, hex with two-glyph+space cells, ascii with `.` for non-printable) and paint each with `shape_line` + one `TextRun` per color span. Color spans per byte class: zero bytes muted, printable default foreground, other accent -- use the theme tokens named in the api-notes (`muted_foreground`, `foreground`, `accent_foreground`; adjust names to what the pinned rev actually has, keep the mapping in one small fn).
4. Selection: for the selection range (from `editor.selection()`), paint per-row `paint_quad(fill(...))` bands behind the hex span and the ascii span BEFORE painting text (selection color = theme selection token). Cursor byte: stronger fill on the active pane cell, outline (`quad` with border) on the inactive pane cell. Nibble caret: when `editor.view_parts().nibble` is Some and pane is Hex, a 2px underline under the active nibble's glyph (High = left glyph, Low = right glyph). Read the nibble via a read-only path: add nothing to hxy-editor; `view_parts()` needs `&mut` (it takes pending scrolls) so DO NOT call it in paint -- instead read the same facts via the existing read-only accessors (`editor.selection()`, `editor.active_pane()`, plus `EditMode`); for the nibble side add a read-only accessor to hxy-editor if none exists: `pub fn nibble(&self) -> Option<NibbleCursor>` in crates/hxy-editor/src/lib.rs delegating to the same logic view_parts uses (editor feature gated; one-line body; mirror it in ViewParts to avoid duplication by having view_parts call it).
5. Column header row and the address gutter paint in the same pass (header = row of column indices in hex, painted at the top, content rows offset one line_h down -- same as egui's layout).

- [ ] **Step 3: Wire a demo grid into the shell**

In `hxy-gpui/src/main.rs`: if a CLI path arg is present, `std::fs::read` it into `hxy_core::MemorySource` (full file-open UX is Task 7; propagate read errors to stderr + exit code 1, no unwrap); else fall back to the placeholder label. When a source exists, the Workspace renders a `HexPane` entity child.

- [ ] **Step 4: Tests**

Painting cannot be pixel-asserted headlessly; test the state layer with `#[gpui::test]` (api-notes "Testing"):

```rust
#[gpui::test]
fn scroll_wheel_moves_and_clamps(cx: &mut gpui::TestAppContext) {
    // build a window with a HexPane over 64 rows of data,
    // simulate ScrollWheelEvent Lines(-3.0), assert scroll_rows == 3.0,
    // simulate a huge positive scroll, assert clamp at 0.0,
    // huge negative, assert clamp at row_count - 1.
}
```

Write it against the real event type per api-notes (`cx.simulate_event(ScrollWheelEvent { .. })` inside a `VisualTestContext`). Expose `#[cfg(test)] pub(crate) fn scroll_rows(&self) -> f32` for assertions. If simulating a raw ScrollWheelEvent against the pane proves impossible at this rev without a full element tree, fall back to unit-testing the scroll-delta-to-rows function directly and note it in the report.

- [ ] **Step 5: Verify + commit**

Run: `cargo test -p hxy-view-gpui && cargo build -p hxy-gpui && cargo clippy -p hxy-view-gpui -p hxy-gpui --all-targets`
Expected: green, zero warnings.

```bash
jj commit -m "feat(hxy-view-gpui): canvas grid painting with selection and scroll"
```

---

### Task 4: Keyboard input through hxy-editor

**Files:**
- Create: `gpui/hxy-view-gpui/src/input.rs`
- Modify: `gpui/hxy-view-gpui/src/pane.rs` (attach `.on_key_down`, effects execution)
- Test: `gpui/hxy-view-gpui/tests/keyboard.rs`

**Interfaces:**
- Consumes: `hxy_editor::{InputEvent, Key, Modifiers, Effect, Disposition}`; gpui `KeyDownEvent`/`Keystroke` (api-notes "Input"); the egui adapter as the semantic reference: `crates/hxy-view/src/input.rs`.
- Produces:

```rust
/// Translate one gpui keystroke into the editor's neutral events.
/// Returns (key event, optional text event) -- the text event
/// reproduces egui's separate Event::Text so ascii typing and vim
/// char-resolution behave identically.
pub(crate) fn translate(keystroke: &gpui::Keystroke) -> (Option<hxy_editor::InputEvent>, Option<hxy_editor::InputEvent>);

impl HexPane {
    /// Feed one key-down through the editor. Returns true when the
    /// editor consumed it (callers stop propagation).
    pub(crate) fn handle_key_down(&mut self, event: &gpui::KeyDownEvent, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) -> bool;
}
```

- [ ] **Step 1: Failing tests first** (`tests/keyboard.rs`, `#[gpui::test]`, api-notes "Testing" section; build a window containing a focused HexPane over `MemorySource::new(vec![0x00; 64])` and use `simulate_keystrokes`)

```rust
#[gpui::test]
fn typing_hex_digits_writes_byte_and_advances(cx: &mut gpui::TestAppContext) {
    // focus pane (hex pane active by default), simulate_keystrokes("a b"),
    // assert byte 0 == 0xAB via pane.editor().source().read(..),
    // assert cursor at offset 1.
}

#[gpui::test]
fn shift_arrow_extends_selection(cx: &mut gpui::TestAppContext) {
    // switch to ascii pane via pane.editor_mut().set_active_pane(Pane::Ascii),
    // simulate_keystrokes("shift-right shift-right"),
    // assert selection anchor 0 cursor 2.
}

#[gpui::test]
fn vim_motion_and_yank_reach_clipboard(cx: &mut gpui::TestAppContext) {
    // set_input_mode(Vim), simulate_keystrokes("y y"),
    // assert cx.read_from_clipboard() text starts with "00 00" (hex yank of row 0).
}

#[gpui::test]
fn cmd_modified_keys_pass_through(cx: &mut gpui::TestAppContext) {
    // simulate cmd-a; assert byte 0 unchanged (no hex 'a' typed).
}

#[gpui::test]
fn ascii_pane_typing_inserts_text(cx: &mut gpui::TestAppContext) {
    // ascii pane active, simulate_keystrokes("shift-z") or "Z" per
    // Keystroke grammar (see api-notes: key_char carries the char),
    // assert byte 0 == b'Z'.
}
```

Consult the api-notes Keystroke grammar for exact `simulate_keystrokes` strings (keys are layout-lowercase, modifiers dash-prefixed).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p hxy-view-gpui --test keyboard`
Expected: FAIL (translate/handle_key_down absent).

- [ ] **Step 3: Implement translate + handle_key_down**

`translate`: map `keystroke.key` strings -> `hxy_editor::Key`: "left"/"right"/"up"/"down" -> arrows, "escape", "tab", "backspace", single ascii letter "a".."z" -> `Key::Letter`, "0".."9" -> `Key::Digit`. Modifiers: `shift`, `platform` -> `command`, `alt` -> `alt` (per api-notes Modifiers fields). Second slot: `keystroke.key_char` when Some and the char `is_ascii_graphic() || == ' '` -> `InputEvent::Text(String)`. `$` arrives as key "4" with shift (verify against Keystroke grammar in notes; if `key` is "$" on shift, map "$" -> `Key::Digit(4)` + shift=true to preserve the editor's LineEnd table).

`handle_key_down`: mirrors the egui adapter contract (crates/hxy-view/src/input.rs): build `editor.input_filter()`; feed the key event, then the text event; execute `apply_input` UNCONDITIONALLY (frame bookkeeping); for `Effect::CopyText(t)` call `cx.write_to_clipboard(gpui::ClipboardItem::new_string(t))`; `cx.notify()`; return whether the key event's Disposition was Consumed. In `render`, attach via `.on_key_down(cx.listener(|this, ev, window, cx| { if this.handle_key_down(ev, window, cx) { cx.stop_propagation(); } }))`.

Note one intentional divergence from egui: the egui adapter runs apply_input once per frame over ALL queued events; here each key-down is its own filter/apply batch. The editor's dispatch semantics are per-event within a batch, and the Insert/Replace Escape latch only matters within one batch, so single-event batches are semantically safe (Escape still pops the mode; there are simply no same-batch followers to drop). State the reasoning in a short comment on handle_key_down.

- [ ] **Step 4: Run tests, clippy; commit**

Run: `cargo test -p hxy-view-gpui && cargo clippy -p hxy-view-gpui --all-targets`

```bash
jj commit -m "feat(hxy-view-gpui): keyboard input through hxy-editor dispatch"
```

---

### Task 5: Mouse: caret, drag select, pane switch

**Files:**
- Modify: `gpui/hxy-view-gpui/src/pane.rs`
- Test: `gpui/hxy-view-gpui/tests/mouse.rs`

**Interfaces:**
- Consumes: `GridGeometry::hit_test`, `FrameInfo` (Task 3), gpui mouse events (api-notes "Mouse"): div-level `.on_mouse_down(MouseButton::Left, ..)`, `.on_mouse_move(..)`, `.on_mouse_up(..)`.
- Produces: `HexPane` fields `drag_anchor: Option<hxy_core::ByteOffset>` and behavior below; no new public API.

Behavior contract (mirror egui hxy-view drag semantics):
- Left-down inside a pane: set active pane from the hit, set caret (anchor == cursor) at hit offset, remember `drag_anchor`, set nibble per hit (`set_selection` + typing-nibble reset comes free via the editor's set_selection), focus the pane, `cx.notify()`.
- Move with left held and `drag_anchor` set: cursor extends to current hit offset (anchor pinned), auto-scroll by one row when the pointer is above/below the grid (clamp).
- Up: clear `drag_anchor`.
- Position mapping: subtract `FrameInfo::content_origin`, add `first_visible_row * line_h + fractional scroll` to y before `hit_test` (write one private helper `hit_at(&self, window_pos) -> Option<GridHit>` used by all three handlers).

- [ ] **Step 1: Failing tests** (`tests/mouse.rs`; simulate_click / simulate_event with MouseDownEvent+MouseMoveEvent+MouseUpEvent per api-notes; assert selection state on the entity)

```rust
#[gpui::test]
fn click_sets_caret_and_pane(cx: &mut gpui::TestAppContext) { /* click a hex cell; assert caret offset, active_pane Hex */ }

#[gpui::test]
fn click_ascii_cell_switches_pane(cx: &mut gpui::TestAppContext) { /* click ascii region; assert Pane::Ascii */ }

#[gpui::test]
fn drag_extends_selection(cx: &mut gpui::TestAppContext) { /* down at byte 3, move to byte 12, up; assert anchor 3 cursor 12 */ }
```

Compute click coordinates from the geometry constants (char_w known from the test font metrics -- read them from FrameInfo after one render, exposed as `#[cfg(test)] pub(crate) fn last_frame(&self) -> Option<FrameInfo>`).

- [ ] **Step 2: Run to verify failure; implement; run; clippy**

Run: `cargo test -p hxy-view-gpui --test mouse`

- [ ] **Step 3: Commit**

```bash
jj commit -m "feat(hxy-view-gpui): mouse caret, drag selection, pane switching"
```

---

### Task 6: Minimap

**Files:**
- Create: `gpui/hxy-view-gpui/src/minimap.rs`
- Modify: `gpui/hxy-view-gpui/src/pane.rs` (right-edge strip in render/paint, interaction)
- Test: extend `gpui/hxy-view-gpui/tests/mouse.rs`

**Interfaces:**
- Consumes: byte-class coloring semantics from `crates/egui_minimap/src/lib.rs` and hxy-view's `HexMinimapSource` adapter (crates/hxy-view/src/lib.rs ~2601-2660) as the behavioral reference.
- Produces: minimap strip (fixed width 96px like egui's default; verify egui_minimap's default and match) painted at the pane's right edge: each minimap row aggregates `ceil(row_count / strip_height_rows)` source rows; per byte-class coloring (zero -> transparent/muted, printable ascii -> theme foreground at low alpha, other -> theme accent at low alpha -- one small fn, same classes as the egui side); viewport indicator = translucent quad over the rows currently visible in the grid; click or drag on the strip scrolls the grid to center that location (set `scroll_rows`, clamp, notify).

- [ ] **Step 1: Failing test** (extend mouse.rs)

```rust
#[gpui::test]
fn minimap_click_scrolls_viewport(cx: &mut gpui::TestAppContext) {
    // pane over 1000 rows of data; click minimap strip at 50% height;
    // assert scroll_rows approximately centers row 500 (tolerance 1 row).
}
```

- [ ] **Step 2: Implement painting + interaction; run tests; clippy**

Painting lives in minimap.rs as a function called from the canvas paint pass with its own strip bounds (grid content width shrinks by strip width + 8px gap). Downsampling: read source bytes per minimap row in one `read` per row (cap read length at 4096 bytes per minimap row; classify and average). On read error paint nothing for that row.

- [ ] **Step 3: Commit**

```bash
jj commit -m "feat(hxy-view-gpui): minimap strip with viewport indicator"
```

---

### Task 7: Shell: open file, status bar, live theme

**Files:**
- Modify: `gpui/hxy-gpui/src/main.rs` (grow into modules if it passes ~300 lines: `src/workspace.rs`)
- Modify: hxy-i18n resources (new keys, existing pattern)

**Interfaces:**
- Consumes: `HexPane` (Tasks 3-6), gpui `PromptOptions`/`prompt_for_paths` (api-notes; the paths-prompt API at the pinned rev), `Theme::sync_system_appearance` + `window.observe_window_appearance` (api-notes "Theme").
- Produces (user-facing behavior, all strings through hxy_i18n):
  - CLI: `hxy-gpui <path>` opens the file (bytes read into MemorySource; errors to stderr, exit 1).
  - `cmd-o` (action + keybinding per api-notes `actions!`/`cx.bind_keys`): native open dialog; chosen file replaces the pane's source via `set_source`.
  - `cmd-alt-v`: toggles `InputMode::Vim`/`Default` on the pane's editor.
  - Status bar (gpui-component `h_flex` + `Label` at window bottom): left = file name or i18n "no file"; middle = cursor offset in hex + selection length when non-caret; right = vim mode indicator (from `editor.vim_state().mode` when input mode is Vim) + dirty marker `*` when `editor.is_dirty()`. All labels re-render via cx.notify from the pane (subscribe: shell holds `Entity<HexPane>` and renders from its state each frame -- reading entity state in the workspace render is the retained-mode idiom; no event plumbing needed for M1).
  - Live theme: observe window appearance; on change call `Theme::sync_system_appearance(Some(window), cx)` so the grid repaints with new tokens.
  - Window title: file name + " - hxy" (set via window.set_window_title or WindowOptions titlebar per notes).

- [ ] **Step 1: Implement; new i18n keys**

Keys (follow existing hxy-i18n naming style; check the .ftl for the real convention and adapt): `gpui-shell-no-file`, `gpui-status-offset`, `gpui-status-selection`, `gpui-status-vim-mode-normal|insert|replace|visual|visual-line` (or reuse existing vim-mode keys if crates/hxy already localizes them -- grep hxy's .ftl first and REUSE existing keys where present rather than minting duplicates).

- [ ] **Step 2: Tests**

Status formatting: pure functions (`fn status_offset_text(selection: Option<Selection>) -> String`) with plain unit tests for caret vs range vs none. Actions/dialog cannot be tested headlessly -- covered at the milestone smoke test.

- [ ] **Step 3: Verify + commit**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`

```bash
jj commit -m "feat(hxy-gpui): file open, status bar, live system theme"
```

---

### Task 8: Milestone gate: verification and adversarial review

**Files:** review-driven.

- [ ] **Step 1: Full sweep**

`cargo test --workspace`, `cargo clippy --workspace --all-targets`, `cargo build -p hxy-gpui --release` (release build sanity).

- [ ] **Step 2: Adversarial whole-milestone review** (fresh subagent, most capable model): the M1 diff against this plan + spec + global CLAUDE.md rules. Named focus areas: egui-parity of geometry rules and input translation (against crates/hxy-view sources), unwrap_or/error handling in file reads and source reads, theme-token usage vs hardcoded colors, i18n coverage of shell strings, no scope creep.

- [ ] **Step 3: Fix findings (one fix wave + scoped re-review), commit fixes.**

- [ ] **Step 4: Hand to the user for the interactive smoke test**

`cargo run -p hxy-gpui -- <some file>` on their desktop: grid renders, typing/vim/selection/minimap/status/theme all feel right, side-by-side with `cargo run -p hxy`.

---

## Self-review notes

- Spec coverage (M1 line in spec): open file (T7), hex/ASCII grid (T3), keyboard nav incl. vim (T4), selection + drag-select (T5), editing (T4 via hxy-editor typing paths), minimap (T6). Shell theming per spec amendment (T7). First feel-comparison possible after T8 smoke.
- Types cross-checked: GridGeometry/GridHit (T2) consumed by T3/T5; FrameInfo produced T3 consumed T5; HexPane API consistent across T3-T7; NibbleCursor read-only accessor added in T3 and used nowhere else yet.
- Known unknowns are delegated to the api-notes file (exact theme token names, Keystroke grammar, prompt_for_paths shape at the pinned rev) with instructions to adapt names while preserving the contract; the notes cite the pinned sources.
