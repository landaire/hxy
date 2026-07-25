# M3: GPUI Analysis Panels and File Lifecycle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bring the GPUI app to analysis-and-files parity: strings/entropy/checksums panels, compare/diff tabs, VFS browser with workspace tabs (nested-dock wrapper), file watching with reload prompts, save/save-as with dirty-close protection, snapshots, unsaved-patch persistence, and global search.

**Architecture:** Same M0-M2 pattern: pure logic moves into `hxy-panels` (egui side refactored onto it, behavior unchanged); GPUI panels are thin renderers emitting the same event vocabulary. The hex-view widget gains the three layers the egui widget already has (row maps with gap rows, hover span, byte styler) so compare and hover integrations work. Workspace-in-a-tab lands via the spike's wrapper-with-guards verdict. Background work uses gpui's background executor (the extracted compute functions are synchronous and thread-safe).

**Tech Stack:** gpui =0.2.2, gpui-component =0.5.1 (Table, Tree, charts, Dialog, notifications), hxy-panels, hxy-editor, hxy-vfs, similar (diff), notify (watch), blake3, suture.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-24-gpui-port-design.md` (M3 scope as amended: visualizers moved to M4 -- they depend on templates).
- Nested workspace layout: GPUI crates in `gpui/` (cargo from `/Users/lander/dev/hxy-gpui/gpui`); shared crates in `crates/` (root workspace). BOTH green (tests + clippy zero warnings + `cargo fmt --check` clean in gpui/) at every task end.
- jj only; `-m` messages; conventional commits; NO AI attribution; ASCII only; no separator comments; no historical framing. Leave `@` described-and-complete at task end (next task stacks with `jj new`).
- Typed thiserror errors; newtypes/enums over bools/primitives; scrutinize new `unwrap_or*`.
- i18n: every user-visible hxy-gpui string through `hxy_i18n::t`/`t_args` (reuse egui keys where they exist -- grep first). Note: egui's global-search UI hardcodes English; do NOT copy that.
- Extractions are verbatim moves with tests; egui app behavior unchanged; re-export shims; the moved/stayed boundary = "compiles without app types".
- The egui reference architecture map used to write this plan is reflected in each task's file:line citations below; where a citation is stale, the current source wins.
- Desktop-only surfaces stay desktop-only (watch/snapshots/save/patch-persist are cfg-gated on the egui side; the gpui app is desktop-only anyway).
- Every task: fresh-subagent review gate; fix loops per SDD skill.

---

### Task 1: Extract analysis + diff logic into hxy-panels; RowSlot to hxy-core

**Files:**
- Modify: `crates/hxy-core/src/` (add `RowSlot` -- moved from `crates/hxy-view/src/lib.rs:741-767` incl. `row_for_byte` if free-standing; hxy-view re-exports for compat)
- Create: `crates/hxy-panels/src/strings.rs`, `crates/hxy-panels/src/entropy.rs`, `crates/hxy-panels/src/checksums.rs`, `crates/hxy-panels/src/diff.rs`, extend `crates/hxy-panels/src/search.rs`
- Modify: `crates/hxy/src/panels/strings.rs`, `panels/entropy.rs`, `panels/checksums.rs`, `compare/mod.rs`, `search/global.rs` (shims + imports), `crates/hxy-panels/Cargo.toml` (deps: similar, web-time, crc32fast/md5/sha1/sha2/blake3/adler -- match the exact crates egui uses), root + gpui workspace Cargo.tomls

**Move (verbatim, with tests):**
- strings: `Encoding`, `StringsConfig`, `StringEntry`, `StringsResult`, `extract`, `Scanner`, `sort_entries`, `printable_codepoint`, `MAX_RESULTS`, `CHUNK_BYTES` (crates/hxy/src/panels/strings.rs:25-413, tests :748-819). `StringsEvent` if app-type-free.
- entropy: `EntropyPoint`, `EntropyState`, `shannon_entropy`, `pick_window_size`, `compute_entropy`, `TARGET_POINTS` (panels/entropy.rs:47-165, tests :327-374).
- checksums: `Algorithm`, `ChecksumConfig`, `ChecksumResult`, `Hasher`, `compute`, `hex_encode` (panels/checksums.rs:27-199, tests :383-448).
- diff: `DiffResult`, `DiffHunk`, `HunkKind`, `diff_op_to_hunk`, `build_row_maps`, `natural_rows`, `interleave_with_gaps`, `PaneFingerprint`, `needs_recompute_debounced`, `DebouncedDecision`, `RECOMPUTE_DEBOUNCE`, the deadline/plain `capture_diff_slices` split (compare/mod.rs:30-620 pure parts, tests :711-775). Consumes `hxy_core::RowSlot` after the move.
- search: `GlobalMatch`, `GlobalSearchState`, `GlobalSearchEvent` (search/global.rs:9-58) join hxy-panels::search.
- STAY: everything touching `HxyApp`, `OpenFile`, egui, `UiInbox`, `egui_plot`, `egui_table`, `ComparePane` (owns egui HexEditor), pickers, render fns.

**Steps:** per-module move -> shim -> root `cargo test --workspace` green (546+ baseline; count moves) -> commit (one per module: `refactor(hxy): extract strings scanning into hxy-panels` etc.; `refactor(hxy-core): host RowSlot for cross-frontend row maps` first). Final: clippy both workspaces; gpui `cargo check`.

---

### Task 2: hxy-view-gpui layers: row maps, hover span, byte styler

**Files:**
- Modify: `gpui/hxy-view-gpui/src/pane.rs`, `paint.rs`, `geometry.rs`, tests

**Interfaces (Produces; mirror the egui builder semantics from crates/hxy-view/src/lib.rs):**
- `HexPane::set_row_map(Option<Vec<hxy_core::RowSlot>>)`: when set, row N renders slot N (Real{offset,len} partial rows allowed, Gap renders nothing but occupies height; addresses from slot offset). Scroll/hit-test/selection paint all honor the map (geometry gains slot-aware row->offset and offset->row lookups mirroring egui row_for_byte).
- `HexPane::set_hover_span(Option<ByteRange>)`: secondary highlight band painted under selection (egui hover_span semantics; theme token distinct from selection).
- `HexPane::set_byte_styler(Option<Box<dyn Fn(u8, ByteOffset) -> ByteStyleOverride + Send>>)` where `ByteStyleOverride { bg: Option<Hsla>, fg: Option<Hsla> }` -- per-byte override consulted in the paint loop (egui ByteStyler semantics: overrides win over class colors).
- All three are None-default and zero-cost when unset.

**Tests:** geometry slot lookups (gap rows skip offsets; partial rows clamp); a gpui::test that a row_map with a Gap renders (paint smoke via rows_visible/FrameInfo) and hit_test on a gap row returns None; hover_span + styler exercised via paint smoke (state-level assertions where pixels unassertable).

Commit: `feat(hxy-view-gpui): row maps, hover span, and byte styler layers`

---

### Task 3: Strings panel (gpui)

**Files:** `gpui/hxy-gpui/src/panels/strings.rs` + workspace/menu/palette wiring

- Per-file tab (like egui Tab::Strings(FileId)): opened from palette/menu for the active file; panel_name "StringsPanel"; dump serializes config + the owning file path (restore re-binds by path or drops).
- Config UI: encoding selector (4), min length, range (goto-expr inputs); Run button; auto-run whole file under the same AUTO_RUN_MAX_BYTES rule (find the constant in crates/hxy/src/app/mod.rs and mirror).
- Compute on gpui background executor calling `hxy_panels::strings::extract`; running spinner; truncation notice (i18n) when `truncated`.
- Results: gpui-component virtualized Table (offset link, text, length), sortable columns mirroring egui sort_entries; row click -> Jump (active file selection + scroll via the established jump path); row hover -> `set_hover_span` on the file's HexPane; filter box.
- Tests: extract already covered in hxy-panels; panel tests: run-over-fixture produces rows; jump sets selection; hover sets span; dump/restore round-trip.

Commit: `feat(hxy-gpui): strings panel`

---

### Task 4: Entropy panel (gpui)

**Files:** `gpui/hxy-gpui/src/panels/entropy.rs` + wiring

- Per-file tab; compute via `hxy_panels::entropy` on background executor; gpui-component chart (line/area) with hex x-axis labels, y fixed [0,8], window-size + mean/max readouts (i18n).
- Egui parity: NO click-to-jump (egui lacks it; record as a shared M4+ enhancement candidate in the report, do not build).
- Normalize the egui convention deviation: this panel uses a proper event enum internally (no &mut bool aping).
- Tests: state round-trip; compute-integration test (fixture bytes -> expected point count via pick_window_size); dump/restore.

Commit: `feat(hxy-gpui): entropy panel`

---

### Task 5: Checksums panel (gpui)

**Files:** `gpui/hxy-gpui/src/panels/checksums.rs` + wiring

- Per-file tab; algorithm checkboxes (7, defaults Sha256+Blake3 like egui), range inputs, Run, auto-run rule; streaming compute via `hxy_panels::checksums::compute` on background executor; per-row copy button -> clipboard (reuse copy plumbing); persisted config in dump.
- Tests: fixture digests (known vectors already in hxy-panels); panel: run -> rows; copy puts hex in clipboard; dump/restore config.

Commit: `feat(hxy-gpui): checksums panel`

---

### Task 6: Compare tab (gpui)

**Files:** `gpui/hxy-gpui/src/panels/compare.rs` (+ submodules as needed) + palette wiring

- `ComparePanel` (panel_name "ComparePanel"): TWO HexPanes side by side (each its own editor over its source), toolbar (sync-scroll toggle, diff-colors toggle, recompute state), bottom hunk table (gpui-component Table: kind, a/b offsets, lengths).
- Diff lifecycle from hxy_panels::diff: fingerprints + 300ms debounce -> background executor recompute (owned Vec snapshots) -> row maps via `build_row_maps(diff, columns)` fed to both panes' `set_row_map`; per-byte diff coloring via `set_byte_styler` (partition_point over hunk ranges, mirroring crates/hxy/src/compare/pane.rs:88-100); hunk hover -> both panes' hover_span; hunk click -> select + scroll both panes (crates/hxy/src/compare/pane.rs:134 semantics).
- Sync scroll: leader/follower mirroring (crates/hxy/src/compare/mod.rs:329).
- Entry points: palette cascade (CompareSideA/B modes picking from open files or a file dialog -- mirror crates/hxy/src/commands/palette Mode compare chain + crates/hxy/src/compare/picker.rs semantics; the dialog route can be cmd-o-style prompt_for_paths).
- Persistence: dump serializes the two sources (paths; editor-side compares re-resolve or drop) -- mirror what egui persists for compare tabs (check tabs/persisted_dock.rs TabSource pairs) within M3 scope: path-vs-path compares restore; open-file-side compares drop on restore (document).
- Tests: diff engine covered in hxy-panels; panel: two fixture buffers -> hunk rows + gap-aligned row maps on both panes; edit one side -> debounced recompute updates hunks; hunk click selects both panes; sync scroll follows.

Commit: `feat(hxy-gpui): compare tab with aligned diff panes`

---

### Task 7: VFS browser + workspace tabs (nested dock productionized)

**Files:** `gpui/hxy-gpui/src/panels/vfs_tree.rs`, `gpui/hxy-gpui/src/panels/workspace_host.rs` (promoted from the spike), workspace wiring

- hxy-vfs is already agnostic: registry with the zip handler registered; on file open, `VfsRegistry::detect(head)` decides whether to offer/auto-mount a workspace (mirror crates/hxy/src/app/mod.rs spawn_workspace flow for when a workspace opens vs a plain file tab -- check the egui trigger: palette "Browse VFS" entry + auto on open? read the source and mirror).
- `WorkspaceHostPanel` (from the dock-spike, guards added per the verdict doc docs/superpowers/plans/2026-07-25-m2-nested-dock-verdict.md): inner DockArea with a VFS tree panel + editor tabs for opened entries; the wrapper's dump() hand-composes inner DockAreaState + mount identity (TabSource-style: parent path + entry paths); guards = the verdict's recommended reactive correction for cross-area drags (implement exactly what the verdict prescribes; if the verdict left options, prefer detect-and-eject: a foreign panel landing in the wrong area is moved back on the next reconcile with a toast).
- VFS tree panel: gpui-component Tree (or List fallback) with LAZY loading (expand-on-demand only, mirroring panels/vfs.rs:172 -- remote mounts must not be walked eagerly), dir/file icons, size column (format_size mirrored), expanded-set persisted in the wrapper dump.
- Entry activation opens the entry as an editor tab INSIDE the workspace's inner dock (streaming::open_vfs equivalent: hxy-vfs read of the entry -> MemorySource for M3; virtual-base hint recorded for M4).
- Feature-flag removal: the dock-spike flag dies; workspace_host becomes a default-build module with the spike's tests promoted + guard tests (cross-area drag simulation per the spike's approach; if only source-analysis was possible, add the reactive-correction test synthetically).
- Tests: mount a fixture zip (hxy-vfs zip handler) -> tree lists entries lazily; activate entry -> inner tab with correct bytes; wrapper dump/load round-trips inner layout + expansion; guard test.

Commit: `feat(hxy-gpui): VFS workspace tabs with nested dock` (+ `refactor(hxy-gpui): promote nested dock spike` first if cleaner)

---

### Task 8: File watching + reload prompts (gpui)

**Files:**
- Modify: `crates/hxy/src/files/watch.rs` (small refactor: replace the `egui::Context` repaint wake with a `Wake` callback abstraction -- `Arc<dyn Fn() + Send + Sync>`; egui side passes ctx.request_repaint, behavior unchanged) -- OR move watch.rs wholesale to a shared location if the egui coupling is only that wake (verify; prefer the smallest change)
- Create: `gpui/hxy-gpui/src/watch.rs` wiring + reload dialog

- Reuse `FileWatcher` (notify + poll worker + VFS fingerprinting) as-is; gpui wake = a channel + cx observer/timer poll draining `drain()` (match the egui cadence).
- Reload flow parity (crates/hxy/src/app/mod.rs:1369-1463 + desktop.rs:873-917): per-file AutoReloadMode (Ask default), prompt dialog (gpui-component Dialog): Reload/Keep My Edits/Ignore -> swap_source vs swap_source_keep_patch on the pane's editor; re-fire the file's analysis panels (strings/checksums/entropy recompute -- the cascade_byte_change equivalent); mark_synced after.
- Removed/renamed files: toast + tab marked (mirror egui's handling -- read what it does for Removed and match).
- Tests: watcher is fs-driven -- integration test with a temp file: modify externally, drain surfaces Modified; the decision application (swap vs keep-patch) tested via direct calls asserting editor state (dirty patch survives KeepEdits, gone on Discard).

Commit: `feat(hxy-gpui): file watching with reload prompts`

---

### Task 9: Save, dirty-close, snapshots, patch persistence (gpui)

**Files:**
- Extract first (root workspace): `write_atomic` + `patch_persist` (all of crates/hxy/src/files/patch_persist.rs) + `SnapshotStore`/`Snapshot` (files/snapshot.rs pure parts) into `crates/hxy-panels/src/files.rs` (or a new `hxy-files` module inside hxy-panels; verbatim, shims, egui green)
- Create: gpui save flow, dirty-close dialog, snapshots dialog

- Save/save-as (cmd-s/shift-cmd-s + File menu items now enabled): mirror crates/hxy/src/files/save.rs -- patched-source read -> write_atomic -> swap_source re-anchor -> title/status update -> mark_synced -> analysis cascade; Save-As via prompt (gpui path save dialog -- check gpui 0.2.2 for prompt_for_new_path or equivalent; if absent, record and use a text-input dialog fallback); VFS in-place writeback via VfsWriter for workspace entry tabs (overwrites only, reject resize like egui save.rs:196).
- Dirty-close protection: closing a dirty FilePanel (cmd-w, X button if interceptable -- check Panel close hooks in 0.5.1; if the X cannot be intercepted, document and guard cmd-w at minimum) -> Save/Don't Save/Cancel dialog; Save only closes on write success (egui close.rs:446-450 rule). Reopen-closed ring buffer (cmd-shift-t, cap 32) storing path + view state.
- Snapshots: capture/rename/delete dialog (per-file), sidecar storage via the extracted SnapshotStore (same $DATA_DIR layout -- SHARE the egui app's snapshot dirs so both apps see the same snapshots? NO -- keep a gpui-suffixed subdir to avoid index.json write races between apps; document); compare picks (Current vs snapshot) spawning a ComparePanel from frozen buffers (Task 6 infra).
- Patch persistence: on quit (and periodically?) mirror egui: on quit only; sidecar per dirty file via extracted patch_persist; on reopen, integrity check -> restore prompt (Clean/Modified/Unknown wording i18n'd).
- Tests: write_atomic + patch_persist + SnapshotStore already tested where moved; gpui: save round-trip on temp file (editor re-anchored, dirty cleared); dirty-close dialog flow (Save success closes, cancel keeps); snapshot capture -> compare opens with frozen bytes; patch sidecar restore round-trip.

Commits: `refactor(hxy): extract file persistence cores into hxy-panels`, `feat(hxy-gpui): save and dirty-close protection`, `feat(hxy-gpui): snapshots`, `feat(hxy-gpui): unsaved patch persistence`

---

### Task 10: Global search (gpui)

**Files:** `gpui/hxy-gpui/src/panels/global_search.rs` + wiring

- Singleton SearchResults panel (opened via palette/menu/shift-cmd-f -- mirror egui's trigger, check shortcuts.rs); reuses hxy_panels::search state types (GlobalSearchState moved in Task 1); query UI mirrors the per-file bar's kind/options.
- Execution: per-file find_all over all open files -- on the background executor (mechanical improvement over egui's UI-thread loop; same results and ordering: files sorted by id). Results list (virtualized): file name + hex offset; click -> focus that file's tab + apply_match_jump semantics.
- ALL strings i18n'd (egui's are hardcoded English -- do not copy; mint gpui-* keys).
- Tests: two fixture files, pattern in both -> aggregated ordered results; jump focuses the right tab + selection.

Commit: `feat(hxy-gpui): global search`

---

### Task 11: Milestone gate

- Sweep both workspaces + release build + fmt --check.
- Adversarial whole-milestone review (most capable model): cross-task seams (per-file analysis panels vs tab lifecycle/close/restore; compare panes vs dock persistence; workspace inner docks vs picker/palette/persistence; watch-reload vs dirty state vs save vs snapshots -- the file lifecycle as ONE state machine), egui-parity spot checks, binding rules, ledger triage. One fix wave + one scoped re-review; controller adjudicates residuals.
- QA checklist for the user (analysis panels, compare, VFS zip workspace, watch/reload, save/dirty-close/snapshots, global search, side-by-side feel).

---

## Self-review notes

- Spec M3 (as amended) coverage: strings (T1+T3), entropy (T1+T4), checksums (T1+T5), compare/diff (T1+T2+T6), VFS browser + workspace tabs (T7), file watching (T8), snapshots + save (T9), global search (T1+T10). Visualizers moved to M4 (template-dependent).
- M2 parked items NOT silently absorbed: atomic layout save, cmd-f focus scope, menu greying, sticky letters remain parked (M3 gate may triage).
- Type consistency: RowSlot moves to hxy-core (T1) before T2 consumes it; ByteStyleOverride defined T2, consumed T6; extracted module paths hxy_panels::{strings,entropy,checksums,diff,files} consistent across T3-T9.
- Known risks: Task 7 (nested dock guards) is the highest-risk task -- the spike verdict governs; Task 9 is the largest and may split further at execution time if an implementer reports it oversized (controller may split into 9a/9b without a new plan cycle).
