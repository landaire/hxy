# M4g: GPUI Theme and Icon Parity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development or superpowers:executing-plans.

**Goal:** hxy-gpui's default dark/light look mirrors hxy-egui's brand identity (violet/lavender/cyan on deep-blue surfaces, six-class byte palette, selection tones) via native gpui-component theming, and every egui phosphor icon site has a working gpui icon -- including fixing the latent bug that ALL current gpui icons render blank (no AssetSource registered; gpui-component bundles no SVGs).

**Reference (read first):** the theme/icon research map at docs/superpowers/plans/2026-08-20-m4g-theme-icon-map.md -- exact egui color values per role per mode (style.rs constants, BytePalette tables, ValueGradient params, modified-byte colors, minimap blends), gpui-component ThemeConfig/ThemeRegistry override mechanics (custom default theme JSON survives appearance toggles; global_mut pokes do NOT), the IconName/lucide inventory, and the 22-glyph egui phosphor usage table with lucide equivalents and the 8 no-match glyphs. Global Constraints = docs/superpowers/plans/2026-08-20-m4a-gpui-templates.md Global Constraints.

---

### Task 1: Icon asset infrastructure (fixes blank icons)

- Add a `gpui::AssetSource` to hxy-gpui (rust-embed or include_dir over a new `gpui/hxy-gpui/assets/icons/` dir) registered via `Application::new().with_assets(...)` in main.rs.
- Vendor the lucide SVGs for every IconName variant the app uses today (13 distinct -- see map 4c) plus the ones Tasks 4 wires (Loader/LoaderCircle, EyeOff, LayoutDashboard, Search, TriangleAlert, CircleX, Close, Chevron*, File/Folder/FolderOpen, Info, Palette). Lucide is ISC-licensed; record license file alongside assets.
- Add phosphor SVGs (MIT) for the 8 glyphs with no lucide match: lock, lock-open, puzzle-piece, house, tree-structure, scroll, image-square, squares-four. Expose them via a small `HxyIcon` enum implementing gpui-component's `IconNamed` trait (map 4b: Icon::path accepts any asset path).
- Verification: a #[gpui::test] asserting the asset source resolves every used path (enumerate IconName variants used + HxyIcon variants; load bytes non-empty); visual smoke screenshot shows real icons.
- Commit: `fix(hxy-gpui): register icon asset source so icons render` + `feat(hxy-gpui): vendor lucide and phosphor icon assets`.

### Task 2: hxy default theme JSON (brand colors, both modes)

- Author `gpui/hxy-gpui/assets/themes/hxy.json` (ThemeConfig schema, map section 3): light+dark ThemeSet marked is_default, mapping style.rs constants onto theme keys: background=SURFACE/#ECE9F4-family, foreground=TEXT/TEXT_LIGHT, muted.foreground=TEXT_DIM/TEXT_DIM_LIGHT, accent.foreground=LAVENDER/LAVENDER_DEEP, selection.background=SELECTION_BG/SELECTION_BG_LIGHT, primary=VIOLET family, border=BORDER/BORDER_LIGHT, danger/warning=error+GOLD tones, plus panel/card/tab/sidebar/table keys approximating PANEL/CARD/CARD_BRIGHT and the hxy_dock_style tab treatment (exact values in the map, section 1a). mono_font stays platform default (egui uses its own embedded font -- check crates/hxy for a custom mono font; if hxy embeds one, defer font bundling with a deviation note).
- Load path: prefer assigning `theme.light_theme`/`theme.dark_theme` to the parsed ThemeConfig at startup before sync_system_appearance (map option 2; simpler than ThemeRegistry file-watching) -- verify appearance toggles keep the custom colors (test: flip appearance via Theme::change in a gpui::test and assert a brand color survives).
- Sanity: all existing panels render legibly with the new palette (contrast of muted on background etc.); screenshot smoke both modes.
- Commit: `feat(hxy-gpui): hxy default theme mirroring the egui look`.

### Task 3: hex-view color parity

- Share the byte-class tables: move BytePalette class color tables + ValueGradient params + MODIFIED_BYTE colors + contrast_text_color from crates/hxy-view (egui) into hxy-core as Rgba constants/fns (verbatim values; egui refactored onto them -- root gates); hxy-view-gpui consumes them: six-class `byte_color` (Null/AllBits/Whitespace/Control/Printable/Extended per map 1b tables, dark/light selected by theme mode -- detect via theme background luminance or a Theme is_dark accessor, check gpui-component for mode), value-gradient palette for the byte_highlight_scheme=Value setting (M4e wired the setting), text-vs-background highlight modes (bg tint + contrast_text_color glyph like egui; the M4a value_palette API covers fg -- extend paint to optionally fill cell bg per class when mode=Background), modified-byte colors from the shared constants (replacing the M4a Task 5 approximation), minimap colored-mode class tinting (CLASS blend 0.65 like egui) and grayscale ramps.
- byte_value_highlight default-off parity: default glyph color = foreground (already true).
- Tests: class resolution table-driven (each class x mode -> expected Rgba), mode switch (settings observer flips palettes), paint smoke.
- Commits: `refactor(hxy): share byte class palettes in hxy-core`, `feat(hxy-view-gpui): six-class byte palette, value gradient, and highlight modes`.

### Task 4: icon wiring parity sweep

- Wire icons at every egui phosphor site's gpui counterpart (map 4a->4c table): status bar (lock/lock-open read-only, warning, tree-structure prefix, watch eye/eye-slash toggle when M4e wired it, spinner Loader for busy), tab titles (template scroll+tree-structure, search Search, workspace house, read-only lock -- tab_name strings gain leading icon where the dock API allows; check Panel::title returning IntoElement -- can include Icon), toasts (puzzle-piece plugin icon, close X -- notification API extras), visualizer header (image-square + close), inspector header (eye), strings panel sort carets, template panel spinner (CIRCLE_NOTCH -> Loader with rotation animation if gpui-component has one -- check Indicator), palette warning entries, welcome recents (File icon per row). Skip areas whose panels do not exist yet (plugins tab -> M4c wires its own icons using this infrastructure).
- Commit: `feat(hxy-gpui): icon parity across panels and status bar`.

### Task 5: milestone gate

Full matrix both workspaces + buck2 + reindeer; dark AND light boot screenshots compared against egui screenshots (launch both apps side by side, capture, eyeball + report); fresh-subagent milestone review (theme JSON key coverage vs map values, icon site coverage vs the 22-glyph table, license files present, no global_mut color pokes); spec status note.
