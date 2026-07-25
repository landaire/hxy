# M2: GPUI Workbench Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the single-pane hxy-gpui shell into a docked workbench: DockArea with File/Welcome/Inspector panels, multiple open files, layout persistence, command palette (with goto and calculator modes), in-file search with replace, vimium-style dock picker, native menus, and toasts.

**Architecture:** Framework-agnostic panel logic (goto parsing, search engine, inspector decoders) is extracted from crates/hxy into a new shared crate `hxy-panels` (M0 pattern: move, re-export, egui side stays green). The gpui app builds panels as gpui-component `Panel` implementations that re-use those cores; the palette is a new overlay frontend over `palette-core`. Reference notes (committed, cited against pinned sources): `docs/superpowers/plans/2026-07-25-m2-gpui-component-notes.md` (dock/modal/input/menu APIs at 0.5.1) and `docs/superpowers/plans/2026-07-24-m1-gpui-api-notes.md`.

**Tech Stack:** gpui =0.2.2, gpui-component =0.5.1 (dock, Dialog via WindowExt, notifications, PopupMenu), palette-core, hxy-editor, hxy-calculator, hxy-i18n.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-24-gpui-port-design.md`. M2 scope: "DockArea workspace, tabs, nested-dock spike, gpui_dock_picker, command palette with calculator, search, goto, data inspector, menus, toasts." NOT in scope: save-to-disk, global/cross-file search tab, VFS, templates, settings persistence beyond dock layout, compare, visualizers.
- Layout facts from the notes files are binding until contradicted by the compiler; where a note conflicts with vendored source, source wins and the discovery is appended to the relevant notes file under its corrections heading.
- Nested workspace layout: GPUI crates live in `gpui/` (nested Cargo workspace, cargo runs from `/Users/lander/dev/hxy-gpui/gpui`); shared crates in `crates/` (root workspace, cargo from `/Users/lander/dev/hxy-gpui`). Both workspaces must be green (tests + clippy zero warnings) at every task end.
- jj only, never git; non-interactive `-m`; conventional commits; NO AI attribution; ASCII only; no separator comments; no historical framing in comments.
- Typed thiserror errors; newtypes/enums over bools/primitives; scrutinize new `unwrap_or*` on fallible data.
- i18n: every user-visible string in hxy-gpui through `hxy_i18n::t`/`t_args` (menus included -- do NOT copy the egui menu.rs habit of hardcoded English). Widget crates (hxy-view-gpui, gpui_dock_picker) carry no user prose.
- The egui app must be behaviorally unchanged by the Task 1 extraction (existing tests green; re-export shims keep crates/hxy imports compiling with minimal diffs).
- Work continues in jj workspace /Users/lander/dev/hxy-gpui stacked on M1 head (commit 174e51ec80d6).
- Every task: tests green in both workspaces, clippy zero warnings, focused commits, fresh-subagent review gate.

---

### Task 1: Extract hxy-panels (goto, search core, inspector decoders)

**Files:**
- Create: `crates/hxy-panels/Cargo.toml`, `crates/hxy-panels/src/lib.rs`, `crates/hxy-panels/src/goto.rs`, `crates/hxy-panels/src/search.rs`, `crates/hxy-panels/src/inspector.rs`
- Modify: root `Cargo.toml` (member + `hxy-panels = { path = "crates/hxy-panels", version = "0.3.0" }`), `gpui/Cargo.toml` (workspace dep `hxy-panels = { path = "../crates/hxy-panels", version = "0.3.0" }`)
- Modify: `crates/hxy/src/commands/goto.rs` (becomes `pub use hxy_panels::goto::*;` shim), `crates/hxy/src/search/mod.rs` (engine/state/event parts move; egui-only parts stay), `crates/hxy/src/panels/inspector.rs` (Decoder trait/impls/state move; `show()` stays), `crates/hxy/Cargo.toml` (dep on hxy-panels)

**Interfaces:**
- Consumes: the current definitions in crates/hxy (goto.rs is 100% pure; search/mod.rs's `SearchState`, `SearchKind`, `NumberWidth`, `SearchEvent`, `SearchSideEffect`, `encode_query`, `find_next`/`find_prev`/`find_all` and the 64KiB chunk scanner are pure over `dyn HexSource`; inspector.rs's `Decoder` trait, `Decoded`, `default_decoders()`, `InspectorState`, all decoder impls are pure).
- Produces: `hxy_panels::goto`, `hxy_panels::search`, `hxy_panels::inspector` re-exporting exactly the moved items under the same names. Deps: hxy-core, hxy-editor (search replace intents reference editor types only if the moved code already does -- check; if replace.rs logic stays in crates/hxy because it couples to `OpenFile`, leave it and move only what is app-type-free), hxy-calculator, jiff, thiserror, tracing. NO egui, NO gpui.

**Rules:** verbatim moves with `#[cfg(test)]` modules included; items coupling to app types (`OpenFile`, `HxyApp`, toasts) STAY in crates/hxy; the moved/stayed boundary is decided by "compiles without app types", and every stayed item is listed in the report. Feature-gate nothing new.

- [ ] **Step 1:** Move goto.rs verbatim; shim + compile + its tests green in new home.
- [ ] **Step 2:** Split search/mod.rs: engine+state+events move, `OpenFile`-coupled helpers stay (report the split). Egui bar/global/modal untouched.
- [ ] **Step 3:** Split inspector.rs: trait/decoders/state move, `show()` and app glue stay.
- [ ] **Step 4:** `cargo test --workspace` (root) green with unchanged counts + hxy-panels new suite; `cargo clippy --workspace --all-targets` clean; `cd gpui && cargo check --workspace` still green (dep registered but unused).
- [ ] **Step 5:** Commits (three focused): `refactor(hxy): extract goto parsing into hxy-panels`, `refactor(hxy): extract search engine into hxy-panels`, `refactor(hxy): extract inspector decoders into hxy-panels`. Also commit the two M2 notes files with the first commit if uncommitted.

---

### Task 2: DockArea workbench with File and Welcome panels

**Files:**
- Modify: `gpui/hxy-gpui/src/workspace.rs` (Workspace owns `Entity<DockArea>`), `gpui/hxy-gpui/src/main.rs`
- Create: `gpui/hxy-gpui/src/panels/mod.rs`, `gpui/hxy-gpui/src/panels/file.rs` (FilePanel), `gpui/hxy-gpui/src/panels/welcome.rs` (WelcomePanel)

**Interfaces:**
- Consumes: notes file sections on DockArea/DockItem/Panel/PanelRegistry/DockEvent (all cited at 0.5.1); existing HexPane.
- Produces:
  - `pub struct FilePanel { pane: Entity<HexPane>, path: Option<PathBuf>, focus_handle: FocusHandle }` implementing `gpui_component::dock::Panel` (`panel_name() -> "FilePanel"`, `title()` = file name, `closable` true, `dump()` serializes the path), `Render`, `Focusable`, `EventEmitter<PanelEvent>`.
  - `pub struct WelcomePanel` (placeholder text via i18n, `panel_name() -> "WelcomePanel"`).
  - Workspace: `DockArea` center = tabs; `open_path(path)` adds a FilePanel tab (cmd-o now ADDS, multiple files supported); closing the last file tab shows Welcome.
  - Layout persistence: `register_panel` for both names; on `DockEvent::LayoutChanged` debounce (500ms via cx timer) then `dump()` -> JSON at the platform data dir (`dirs`-style path: use the same location scheme crates/hxy uses for its persisted state -- check `crates/hxy/src/settings/persist` for the dir convention and mirror it under a `gpui` filename); `load()` at boot with fallback to default layout; FilePanel restore re-reads the dumped path (missing/unreadable file -> panel dropped with a toast, not a crash).
- Status bar moves to reflect the ACTIVE file panel (dock active tab), not a single global pane; vim toggle and typing focus route to the active FilePanel.

**Tests:** `#[gpui::test]`: open two files -> two tabs, active switches; close tab; dump/load round-trip restores tab count and paths (write to a temp dir, not the real data dir -- make the persist path injectable). Keyboard typing still reaches the active pane (regression of the M1 focus tests, adapted).

- [ ] Commit: `feat(hxy-gpui): dock workbench with file and welcome panels`
- [ ] Commit: `feat(hxy-gpui): dock layout persistence`

---

### Task 3: Inspector panel

**Files:**
- Create: `gpui/hxy-gpui/src/panels/inspector.rs`
- Modify: workspace wiring (right dock, toggleable)

**Interfaces:**
- Consumes: `hxy_panels::inspector::{Decoder, Decoded, default_decoders, InspectorState}`; active FilePanel's editor selection.
- Produces: `InspectorPanel` (Panel impl, `panel_name() -> "InspectorPanel"`): endian + radix toolbar (gpui-component buttons/dropdown), a grid of decoder name -> decoded value for a 16-byte window at the active file's caret, color swatch rendering for `Decoded::Color`. Refresh: read the active panel's editor each render (retained: workspace re-renders on pane notify; subscribe to the dock's active-panel changes per notes). Toggle via `cmd-i` action + View menu (Task 6). All labels i18n (reuse egui's existing inspector keys if present in the .ftl -- grep first).

**Tests:** decoder table already tested in hxy-panels; panel test: caret move updates the decoded window (drive via keystroke on the file panel, read inspector state); endian flip changes u16 decode.

- [ ] Commit: `feat(hxy-gpui): data inspector panel`

---

### Task 4: In-file search bar with replace

**Files:**
- Create: `gpui/hxy-gpui/src/panels/search_bar.rs`
- Modify: `gpui/hxy-gpui/src/panels/file.rs` (bar slot below the pane, cmd-f toggles)

**Interfaces:**
- Consumes: `hxy_panels::search::{SearchState, SearchKind, NumberWidth, encode_query, find_next, find_prev, find_all}` (exact names per Task 1's move); gpui-component `InputState`/`Input` (note: 0.5.1 `InputEvent::Change` carries NO payload -- read `state.value()` in the subscriber, per notes); Dialog via `WindowExt::open_dialog` for the replace-all confirm; egui reference for UX: `crates/hxy/src/search/bar.rs` (controls inventory) and `crates/hxy/src/app/mod.rs:1725-1813` (event application: jump = set_selection + scroll).
- Produces: cmd-f opens the bar (kind selector Text/Hex/Number, case/endian/width options per SearchState fields, match counter "N of M" i18n'd, next/prev buttons + Enter/shift-Enter, Esc closes and refocuses the grid); replace field with replace-current and replace-all (confirm dialog when >1 match; length-mismatch splice warning mirrors egui modal.rs semantics); match jump sets the editor selection to the match range and scrolls (reuse the M1 pending-scroll path).
- Match highlighting = selection jump only (egui parity; no byte_styler).

**Tests:** `#[gpui::test]`: type a hex query, Enter jumps selection to first match and counter updates; next wraps with toast-free state change (wrap toast comes in Task 6 -- emit the event now, surface later); replace-current rewrites bytes via the editor (assert bytes + single undo entry for replace-all via splice_many).

- [ ] Commit: `feat(hxy-gpui): in-file search bar with replace`

---

### Task 5: Command palette

**Files:**
- Create: `gpui/hxy-gpui/src/palette/mod.rs` (overlay widget over palette-core), `gpui/hxy-gpui/src/palette/modes.rs` (mode enum + entry builders), `gpui/hxy-gpui/src/palette/apply.rs` (action dispatch into Workspace)

**Interfaces:**
- Consumes: `palette_core::{State, Entry, filter_and_sort, MatchResult, Outcome, DismissReason}`; `hxy_panels::goto` parsers; `hxy_calculator::evaluate_str_with`; egui references for UX parity: `crates/egui-palette/src/lib.rs` (anchor TopCenter y=72, backdrop dismiss, ghost-completion selection semantics) and `crates/hxy/src/commands/palette/mod.rs` (Mode::parent Esc-pop cascade).
- Produces:
  - A custom overlay (deferred/anchored layer per notes' overlay guidance; NOT the Dialog component -- the palette needs top-center anchoring and per-keystroke filtering): backdrop click dismisses fully; Esc pops one cascade level (Mode::parent) else closes; Input at top (theme icon prefix), filtered rows (title, subtitle, shortcut hint via `Kbd::binding_for_action` where an action exists, disabled rows greyed and inert), arrow/Enter navigation, match-char highlighting from `MatchResult::match_indices`.
  - `enum PaletteMode { Main, GoToOffset, GoToAddress, SelectFromOffset, SelectRange, SetColumns }` with `parent()`; Main entries (all i18n): Open File, Close Tab, Toggle Vim Mode, Toggle Inspector, Go To Offset..., Select From Offset..., Set Columns..., Copy Selection As Hex, Copy Selection As Bytes, plus the calculator prefixes: query starting `@expr` -> evaluated go-to entry, `=expr` -> copy-result entries (decimal/hex/binary rows), both via `evaluate_str_with` with a no-op resolver (template fields arrive M4) and `bypass_filter` semantics from palette-core State.
  - Argument modes parse via `hxy_panels::goto::{parse_offset_expr, parse_count_expr, parse_range_expr}`; invalid input renders one disabled "invalid: <err>" row (i18n with arg).
  - cmd-shift-p opens Main (match the egui app's binding -- check `crates/hxy/src/app/shortcuts.rs` for the actual palette chord and mirror it; also cmd-g -> GoToOffset if the egui app binds it -- mirror what exists, do not invent).
  - Apply: actions dispatch on Workspace (open dialog, close active tab, toggles, goto -> active pane set_selection+scroll, set columns -> pane columns + notify, copy -> clipboard via existing formatting helpers).
- Ghost completion field is present in State but no resolver exists yet in gpui (template paths are M4): leave `completion_suggestion` unset; do NOT stub fake completions.

**Tests:** unit: entry builders per mode (counts, disabled rows for no-selection copy); goto mode parse-and-jump end-to-end via `#[gpui::test]` keystrokes (open palette, type `+10`, Enter, assert caret moved); calculator `=2+2` produces copyable rows; Esc pops GoToOffset -> Main -> closed.

- [ ] Commit: `feat(hxy-gpui): command palette with goto and calculator modes`

---

### Task 6: Menus, actions, toasts

**Files:**
- Create: `gpui/hxy-gpui/src/menu.rs`
- Modify: workspace/main wiring; search bar + palette emit toasts

**Interfaces:**
- Consumes: gpui 0.2.2 `App::set_menus`/`Menu`/`MenuItem::action` (native macOS per notes; other platforms get the data only -- acceptable, macOS is the target); gpui-component `Notification` + `window.push_notification`; the egui menu inventory (`crates/hxy/src/menu.rs:28-58`) as the parity checklist.
- Produces:
  - Native menu bar (ALL titles through hxy_i18n; the egui side's hardcoded English is a known deviation NOT to copy): App (About/Quit predefined), File (New disabled-stub omitted -- only ship what works: Open cmd-o, Close Tab cmd-w), Edit (Undo cmd-z, Redo shift-cmd-z, Toggle Edit Mode cmd-e, Copy Bytes cmd-c, Copy Hex shift-cmd-c), View (Toggle Inspector cmd-i, Toggle Vim). Every item dispatches a gpui action also bound in the keymap, so menus and shortcuts share one action set; per-item enabled state where gpui 0.2.2 supports it (check source; if dynamic enable/disable is unsupported at 0.2.2, ship always-enabled items that no-op gracefully and record the limitation in the notes file).
  - Toasts: search wrap-around, replace-N-done, file-open errors (replacing the Task 2 open-error inline text if cleaner), layout-restore failures. Use Notification::info/success/warning/error mapping to the egui toast kinds.
- Undo/Redo/copy act on the active FilePanel's editor (existing hxy-editor APIs).

**Tests:** action-level `#[gpui::test]`: dispatch Undo action after typing reverts byte; Copy Hex puts formatted text in clipboard (reuse egui's format: space-separated uppercase hex -- same as vim yank formatting). Menu bar itself is macOS-native and untestable headlessly: state that in the report.

- [ ] Commit: `feat(hxy-gpui): native menus, shared actions, toasts`

---

### Task 7: gpui_dock_picker crate

**Files:**
- Create: `gpui/gpui_dock_picker/` (crate: Cargo.toml, src/lib.rs), registered in gpui/Cargo.toml members + workspace deps
- Modify: `gpui/hxy-gpui` wiring (activation keybinding; egui side uses a chord -- check `crates/hxy/src/tabs/pane_pick.rs` + `crates/hxy/src/app/shortcuts.rs` for the trigger and mirror it)

**Interfaces:**
- Consumes: DockArea's panel enumeration (notes: walk the DockItem tree / TabPanel lists; determine at implementation time what 0.5.1 exposes publicly for iterating panels and their screen bounds -- if bounds are not exposed, overlay letters on the TAB STRIP entries instead of panel centers and record the compromise); `egui_dock_picker`'s UX as reference (letter badges, Esc cancels, timeout).
- Produces: `pub struct DockPicker` reusable component: `DockPicker::activate(dock_area: &Entity<DockArea>, on_pick: impl Fn(PickTarget))` style API (exact shape driven by what 0.5.1 allows; keep hxy-gpui-agnostic -- no hxy deps in the crate); renders an overlay layer with a-z badges over each pickable target; next keypress focuses that panel (via its FocusHandle / DockArea activation API); Esc/click cancels. `PickTarget` enum rather than raw indices.
- In hxy-gpui: bind the mirrored chord; picking a FilePanel focuses its HexPane.

**Tests:** `#[gpui::test]` with a 3-panel dock: activate -> overlay state lists 3 targets with distinct letters; simulate letter keystroke -> active panel changes; Esc cancels. If overlay hit/paint cannot be asserted headlessly, assert the state machine (targets, chosen focus) and say so.

- [ ] Commit: `feat(gpui_dock_picker): vimium-style pane picker for gpui-component docks`

---

### Task 8: Nested-dock spike (workspace-in-a-tab)

**Files:**
- Create: `gpui/hxy-gpui/src/panels/workspace_spike.rs` (spike only; behind `#[cfg(feature = "dock-spike")]` on hxy-gpui or a bin target -- keep it OUT of the default build)
- Modify: none outside the spike

**Interfaces:**
- Consumes: notes verdict (nesting unsupported upstream: DockArea is not a Panel; wrapper must render an inner `Entity<DockArea>` and hand-serialize `DockAreaState` into its `dump()` json; cross-area tab drags unguarded at tab_panel.rs:924-957).
- Produces: a WRITTEN VERDICT, not product code. The spike builds a `WorkspaceHostPanel` wrapping an inner DockArea with two dummy panels; exercises: (a) rendering/focus inside a tab, (b) dump/load round-trip through PanelInfo::Panel json, (c) what happens on a tab drag from inner to outer area (document the failure mode). Timebox: if (a) or (b) fundamentally fails, stop and record. Deliverable: `docs/superpowers/plans/2026-07-25-m2-nested-dock-verdict.md` with findings, the recommended M3+ approach (wrapper with guards vs flat-dock emulation of workspaces vs upstream patch), and the spike code compiled under the feature flag with a `#[gpui::test]` for (b) if it works.

- [ ] Commit: `feat(hxy-gpui): nested dock spike behind feature flag` + `docs: nested dock spike verdict`

---

### Task 9: Milestone gate

- [ ] Sweep: root + gpui workspaces tests/clippy; `cargo build -p hxy-gpui --release`.
- [ ] Adversarial whole-milestone review (most capable model): cross-task seams (dock focus vs pane focus vs palette overlay focus; action set collisions; persistence robustness), egui-parity of palette/search/goto behavior against the mapped references, binding user rules over the whole diff, ledger deferred-minors triage. One fix wave + one scoped re-review, controller adjudicates residuals.
- [ ] QA checklist for the user (multi-file docking, drag-dock, layout restore after relaunch, palette flows, search/replace, picker, menus, toasts, inspector) ordered highest-risk first.

---

## Self-review notes

- Spec M2 line coverage: DockArea workspace + tabs (T2), nested-dock spike (T8), gpui_dock_picker (T7), command palette with calculator (T5), search (T4), goto (T1+T5), data inspector (T1+T3), menus (T6), toasts (T6).
- Deliberate deferrals vs the egui app, recorded here: global search tab, QuickOpen/Recent/Templates palette modes, virtual-base GoToAddress semantics (gpui has no virtual base yet -- GoToAddress mode ships parsing-complete but equals GoToOffset until M3/M4; keep the mode so muscle memory matches), ghost completion resolver (M4), Save/dirty-close modals (M3).
- Type-consistency: FilePanel/InspectorPanel/WelcomePanel names used consistently across T2/T3/T6/T7; PaletteMode::parent matches egui Mode::parent semantics; hxy_panels module paths fixed in T1 and consumed in T3/T4/T5.
