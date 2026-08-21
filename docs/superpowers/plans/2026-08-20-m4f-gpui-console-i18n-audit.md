# M4f: GPUI Console, i18n Sweep, and Final Parity Audit Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development or superpowers:executing-plans.

**Goal:** Close the GPUI port: a Console tab collecting plugin/template diagnostics (rerouting the M4c ConsoleLog seam), a complete i18n sweep (every user-visible string localized + system-locale init at startup), and a final feature-parity audit against the egui app that either fixes or explicitly documents every remaining gap.

**Reference map (read first):** docs/superpowers/plans/2026-08-20-m4-plugins-ipc-settings-console-map.md section (4) CONSOLE AND WELCOME TABS (ConsoleEntry{timestamp, severity, context, message}, ConsoleSeverity{Info,Warning,Error}, console_log auto-open-on-Error, console_ui 4-col grid time/severity-icon/context/message, CONSOLE_CAPACITY; welcome_ui app-name/tagline/recents). Global Constraints = docs/superpowers/plans/2026-08-20-m4a-gpui-templates.md Global Constraints.

**Carry-in items (from prior milestones, address here):**
- gpui main.rs has no `hxy_i18n::init_from_system_locale()` call (egui calls it at startup to follow the OS language); the gpui app relies on lazy default init -> likely always English. FIX in the i18n task.
- Plugin command icons not rendered in the palette (egui shows a puzzle-piece glyph); M4g gave us HxyIcon::PuzzlePiece + palette-core has no icon field pre-0.7 -> decide (wire if the palette row supports an icon token, else document).
- NSServices right-click "Open in hxy" provider not ported to gpui (needs an Info.plist + gpui has no bundle here); document as a known gap.

---

### Task 1: Console tab

**Files:** create gpui/hxy-gpui/src/panels/console_view.rs; modify gpui/hxy-gpui/src/panels/mod.rs (register), persist.rs (AlwaysKeep), workspace.rs (open/singleton/toggle + the PluginConsole impl reroutes here), plugins.rs (ConsoleSeverity: unify — move to a shared spot or hxy-core; the M4c local enum + template diagnostics both feed the console), menu.rs + palette (ToggleConsole / OpenConsole), templates.rs (route template Error/Warning diagnostics to the console too), crates/hxy-i18n ftl (console-* keys — check which egui keys exist: console-empty exists; add gpui console chrome keys).

- `ConsolePanel` (CONSOLE_PANEL_NAME, panel_kind AlwaysKeep, singleton) with the egui console_ui layout: bottom-stuck scroll + rows of (HH:MM:SS time, severity icon via IconName Info/TriangleAlert/CircleX, muted context, message), severity colors from theme (warn/error). Empty state = t("console-empty"). Capacity-bounded VecDeque<ConsoleEntry> (CONSOLE_CAPACITY) stored on Workspace (the console is app-global, not per-file) — the panel reads it via a global or a Workspace-held entity.
- Rework the M4c `PluginConsole` seam impl on Workspace: instead of (or in addition to) toasts, push a ConsoleEntry; auto-open the console on Error (egui console_log auto-opens; mirror — open the Console tab on first Error). Template diagnostics (currently error toast + tracing from M4a) also push ConsoleEntry (Error diagnostics still toast; all severities log to console).
- Open paths: View menu ToggleConsole + palette OpenConsole/ToggleConsole action + keybinding if egui has one (check egui menu for console shortcut; mirror).
- Tests: registry build, singleton open/focus, AlwaysKeep prune, console_log pushes an entry + auto-opens on Error, capacity eviction, a plugin op failure (M4c path) lands a console Error entry.

Commit: `feat(hxy-gpui): console tab for plugin and template diagnostics`.

### Task 2: i18n sweep + system-locale init

**Files:** gpui/hxy-gpui/src/main.rs (add init), any gpui file with a hardcoded user-visible string, crates/hxy-i18n ftl (missing keys), palette (plugin command icon decision).

- Startup: add `hxy_i18n::init_from_system_locale()` (or the exact fn egui uses — check crates/hxy/src/main.rs:39 and hxy-i18n/src/lib.rs) at the top of gpui main(), before any UI. Verify the app now follows the OS language (test: set locale, assert a known key resolves to the localized value — or at minimum that init is called and non-panicking; en-US is the only locale so behavior is unchanged, but the call must be present for future locales).
- Sweep: grep every gpui/hxy-gpui + gpui/hxy-view-gpui source for hardcoded English string literals reaching the UI (SharedString::from("..."), .child("...literal..."), Label::new("..."), Button labels, tooltips, toast text, panel titles, menu items, dialog text). Each must route through hxy_i18n::t / t_args. Add any missing keys to en-US/main.ftl. EXCLUDE: plugin-authored strings (pass through), debug/tracing logs, test code, element ids, format specifiers. Produce a list of what was found + fixed.
- Plugin command icon: if the palette row can render a leading icon token (M4g added ICON_WARNING token support to palette rows), add an ICON_PLUGIN token -> HxyIcon::PuzzlePiece for plugin command entries; else document why not.
- Tests: a test asserting init_from_system_locale is wired (or that a representative sample of previously-hardcoded strings now come from t()); i18n crate still green.

Commit(s): `feat(hxy-gpui): initialize i18n from system locale`, `i18n(hxy-gpui): route remaining hardcoded strings through hxy_i18n`.

### Task 3: Final feature-parity audit

This is the capstone — a comprehensive adversarial audit, best run as a dedicated fresh subagent with broad read scope.
- Enumerate every egui app feature (walk crates/hxy: tabs/panels, menu items, palette commands, keyboard shortcuts, status bar elements, dialogs, settings, file lifecycle, VFS, templates, visualizers, plugins, IPC) and produce a PARITY MATRIX: feature | egui | gpui | status (parity / documented-deviation / gap).
- For each GAP or undocumented deviation: either fix it if quick (a missing palette command, an unlocalized string, a missing keybinding) or document it in a durable place (spec deviations section + code comment).
- Cross-check the accumulated deviation ledgers from M4a-M4e (settings.rs module doc, template deferrals, visualizer deviations, theme/icon deviations, IPC NSServices) into ONE consolidated "known deviations from egui" section in the port spec.
- Produce docs/superpowers/plans/2026-08-20-m4f-parity-matrix.md as the deliverable artifact.

Commit: `docs: gpui-vs-egui parity matrix and consolidated deviations` + any quick-fix commits.

### Task 4: Milestone gate + port completion

- Full matrix both workspaces + buck2 (`nix develop --command buck2 build //:hxy`) + reindeer drift + wasm check.
- Boot smoke: app opens, Console tab reachable and shows a diagnostic when a template with an error runs; dark+light still correct.
- Fresh-subagent milestone review (console rerouting completeness, i18n sweep completeness — grep for stragglers, locale init correctness, parity-matrix honesty — spot-check 5 "parity" claims against actual code).
- Spec: mark M4 complete; the consolidated deviations section is the port's honest parity statement.
- Update the port spec Status line from "approved (pending spec review)" to reflect M4 completion.
