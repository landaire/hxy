# M4g reference: theme and icon parity map (verified 2026-08-20)

## 1. EGUI DEFAULT LOOK

### 1a. UI chrome Visuals (crates/hxy/src/style.rs)
Two hand-authored Visuals (dark+light) + base_style + egui_dock override; applied via set_style_of(Theme::Dark, hxy_style()) / (Theme::Light, hxy_light_style()) + set_theme(ThemePreference::System) at app/desktop.rs:72-74 and app/wasm.rs:49-51. System-follow delegated to egui.

Dark constants (style.rs:22-49): SURFACE #0A0B10, PANEL #101119, CARD #161722, CARD_BRIGHT #1F1F2E, BORDER #302F44, BORDER_BRIGHT #5C4E7D, VIOLET #9D56FF, LAVENDER #B5A4E0, CYAN #60CDD7, GOLD #F5CC4E, TEXT #E6E5F0, TEXT_DIM #9795AA, TEXT_BRIGHT #FAF8FF, SELECTION_BG #522A7C, SELECTION_STROKE=LAVENDER.
Light (style.rs:54-73): SURFACE_LIGHT #ECE9F4, PANEL_LIGHT #F7F5FC, CARD_LIGHT #FFFFFF, CARD_BRIGHT_LIGHT #ECE7F7, BORDER_LIGHT #D3CEE3, BORDER_BRIGHT_LIGHT #A794CC, LAVENDER_DEEP #7B5FC0, CYAN_DEEP #0F7E8C, GOLD_DEEP #9E7806, TEXT_LIGHT #23212E, TEXT_DIM_LIGHT #6A6680, TEXT_BRIGHT_LIGHT #0F0D17, SELECTION_BG_LIGHT #DCC9F5, SELECTION_STROKE_LIGHT=LAVENDER_DEEP.

dark_visuals() (127-224): inactive #1A1B27/#181924, hovered #27233A/#2B2640 VIOLET stroke, active #362450/#3D2658 LAVENDER stroke; selection {SELECTION_BG, LAVENDER} (178); hyperlink CYAN (180); faint_bg violet@a14 (181); extreme_bg SURFACE (182); code_bg #12131C (183); warn GOLD (185); error #FF5C80 (186); window/panel_fill PANEL (195/199); window_stroke BORDER (196); text_cursor w2 LAVENDER (211).
light_visuals() (226-326): inactive #F3F0FA, hovered #E9E2F8 VIOLET, active #DFD2F6 LAVENDER_DEEP; selection {SELECTION_BG_LIGHT, LAVENDER_DEEP} (277-280); hyperlink CYAN_DEEP; extreme_bg SURFACE_LIGHT; code_bg #EEEAF8; warn GOLD_DEEP; error #C42046; cursor LAVENDER_DEEP (313).
hxy_dock_style() (346-384): tab bar surface tone; active/focused tab panel tone; active outline VIOLET; focused outline LAVENDER(_DEEP); hovered outline border_bright; inactive outline border. Applied desktop.rs:1853, wasm.rs:566,797, desktop_tab_viewer.rs:513.

### 1b. Hex view (crates/hxy-view/src/lib.rs) - derives from live visuals
RowColors (1161-1169): text=text_color, weak=weak_text_color, selection_bg=selection.bg_fill (#522A7C dark / #DCC9F5 light), selection_fg=selection.stroke.color (LAVENDER/LAVENDER_DEEP), cursor_stroke w1.5 strong_text_color, hover_stroke w1.0 weak*0.9.
Cursor marks (1486-1590): inactive pane dimmed {width, weak} (1499-1503); rounded r2; hover hover_stroke; EOF caret weak*0.6 (1553); nibble weak*0.7 (1588). Hover band selection_bg*0.45 (1360-1376) = source of gpui HOVER_TINT_ALPHA.
Modified bytes (crates/hxy/src/view/hex_body.rs:22-25): MODIFIED_BYTE_BG rgba_premul(0x80,0x10,0x10,0xB0) text-mode; MODIFIED_BYTE_FG #FF5A4A bg/disabled mode; applied hex_body.rs:91-97,157-178 from editor.modified_ranges().
Address gutter: labels+header weak_text_color (2396,2472); header bg panel_fill (2395); divider weak*0.5 (2436).
BytePalette (1906-1989) for_theme_and_mode(dark,mode) (1924-1931); ByteClass::of (2017-2028): Null 0x00, AllBits 0xFF, Whitespace \t\n\r, Control 0x01-0x1F|0x7F, Printable 0x20-0x7E, Extended 0x80-0xFE.
BG_DARK (1938): null #3C3C40, all_bits #8C6E2A, ws #2E4E78, printable #2E6840, control #7A3E3E, ext #663C7A.
BG_LIGHT (1947): null #DCDCDC, all_bits #F5D76E, ws #B4D2F0, printable #BEEBC8, control #F0BEBE, ext #E1C3F0.
TEXT_DARK (1961): null #909094, all_bits #D6AE60, ws #80B0DA, printable #88C898, control #DA8A8A, ext #BC95D2.
TEXT_LIGHT (1970): null #787878, all_bits #B47814, ws #1E5AB4, printable #1E823C, control #B43232, ext #8228AA.
Active only when byte_value_highlight on (hex_body.rs:49); default glyph color = colors.text.
ValueGradient (1862-1904): hue=byte/256*360; S/L: BG_DARK {0.55,0.32}, BG_LIGHT {0.5,0.78}, TEXT_DARK {0.75,0.68}, TEXT_LIGHT {0.7,0.4} (1869-1872); hsl_to_rgb 1889.
HighlightPalette (1837-1857): Class|Value|Custom(Arc<[Color32;256]>); default via for_theme_and_mode(visuals.dark_mode, mode) (423).
contrast_text_color (1994-2004): luminance-interp grey (240 -> 30) for bg-mode glyphs.
Minimap (2110-2168, source 2033-2107): accent=selection.bg_fill (2138); fallback=text_color (2148); colored mode palette.color_for(byte)*0.65 (2088-2103); grayscale ramps dark 40-230 light 40-220 (2362-2367); field colors override (2352-2360); hover overlay accent*0.35 (2228).

## 2. GPUI CURRENT LOOK
PaintColors::from_theme (gpui/hxy-view-gpui/src/paint.rs:673-684): foreground<-theme.foreground, muted<-muted_foreground, accent<-accent_foreground, selection<-selection, hover=selection.opacity(0.45), background<-background. pane.rs:611-613 fonts from theme; pane bg background (625).
byte_color (paint.rs:651-659): ONLY 3 classes: 0->muted, printable->foreground, else->accent. glyph_color (644-649) prefers value_palette. Cursor fill accent*0.4 (51,509); inactive cursor accent outline (520); nibble accent (620); header/address muted (420,424,531); header band background (413).
No hardcoded prod colors; minimap alphas CLASS 0.55, STRIP_BG 0.06, INDICATOR fill 0.18 / outline 0.55 (minimap.rs:54-60), colors from muted/foreground/accent (112,168-190,211-212).
gpui-component 0.5.1 defaults (default-theme.json via registry.rs:14-40, schema.rs:415): light/dark: foreground #0a0a0a/#fafafa, muted_foreground #737373/#737373, accent_foreground #171717/#fafafa, selection #55a0fc/#1d4ed8, background #ffffff/#0a0a0a, border #e5e5e5/#262626, primary #171717/#fafafa. So current gpui = monochrome + blue selection; no violet identity; 3 byte classes; no modified/class/value palettes.
Fonts (theme/mod.rs:189-197): mono Menlo/Consolas/DejaVu 13px; ui .SystemUIFont 16px; radius 6.

## 3. GPUI THEMING CAPABILITY
Theme is gpui Global (mod.rs:98); cx.theme() read (28-37,103-105); Theme::global_mut (108-111); Deref to ThemeColor (84-96).
sync_system_appearance (129-138) -> change (150-170) re-runs apply_config(dark_theme|light_theme) (161-165) rebuilding EVERY field (schema.rs:415-441,645-704) -- direct global_mut pokes are WIPED on appearance change. hxy-gpui calls sync at startup + observe_window_appearance (main.rs:46-49).
Supported override: custom ThemeConfig JSON. Options: (1) ThemeRegistry watch_dir (registry.rs:98-118) with "is_default": true replacing default_themes[mode] (256-259), observer re-applies (47-73); (2) assign theme.light_theme/dark_theme to custom Rc<ThemeConfig> then Theme::change -- survives toggles; (3) re-poke in observer (fragile).
Schema (theme/schema.rs, 723 lines): dotted keys "accent.foreground", "selection.background", "muted.foreground"; try_parse_color #RRGGBB/#RRGGBBAA (407-411); also font_family/size, mono_font_*, radius, radius_lg, shadow, highlight. Absent keys fall back to ThemeColor::default()/computed (418-439, e.g. muted_foreground = muted.blend(foreground.opacity(0.7))).

## 4. ICON INVENTORY
### 4a. egui phosphor usage (dep Cargo.toml:60; font registered app/mod.rs:2554). 22 distinct glyphs:
Tabs (desktop_tab_viewer.rs): TREE_STRUCTURE (178 template, 587 VFS), MAGNIFYING_GLASS (182 search results; wasm.rs:759), HOUSE (574 workspace), LOCK (577 read-only).
Status/console (app/mod.rs): CIRCLE_NOTCH (1944, 3581 spinner/busy), WARNING (1978, 3549, 3585), SQUARES_FOUR (3367 panels toggle), TREE_STRUCTURE (3372, 3602), EYE/EYE_SLASH (3381 watch toggle), LOCK (3452,3459,3590,3605), LOCK_OPEN (3462), INFO (3548), X_CIRCLE (3550).
Toasts (toasts.rs): PUZZLE_PIECE (268), X (280).
Visualizer (visualizers/mod.rs): IMAGE_SQUARE (140), X (143).
Inspector (panels/inspector.rs): EYE (24).
Strings (panels/strings.rs): CARET_DOWN/UP (300/302 sort).
VFS (panels/vfs.rs): FOLDER (161), FILE (181).
Template (panels/template.rs): SCROLL (79,758), X (82,849), PAINT_BUCKET (91), X_CIRCLE (134), WARNING (135), INFO (136,873), CARET_RIGHT/DOWN (505/507), CIRCLE_NOTCH (844), IMAGE_SQUARE (909).
Search bar (search/bar.rs): CARET_DOWN (132), CARET_UP (135), X (168). Global search (search/global.rs): MAGNIFYING_GLASS (84), X (91). Palette (commands/palette/mod.rs): WARNING (767). wasm picker: FILE (1078).

### 4b. gpui-component 0.5.1 icons (src/icon.rs)
IconName enum 84 variants (25-112) -> SVG paths "icons/<lucide>.svg" (121-213); LUCIDE names. Arbitrary SVGs OK: Icon::path (273-276); IconNamed trait (12-21) for app enums; Icon = svg() wrapper tinted by text_color (227-235,319-372).
CRITICAL: crate ships ZERO svg files and registers NO AssetSource; consuming app must provide gpui AssetSource serving icons/*.svg. hxy-gpui registers none (main.rs:32 no .with_assets) -> ALL current gpui icons render blank.

### 4c. gpui usage today (13 IconNames): vfs_tree FolderOpen/Folder/File/ChevronDown/ChevronRight; global_search Search; template_view Palette/Close/ChevronDown/ChevronRight/CircleX/TriangleAlert/Info/Eye; palette Search; search_bar ChevronDown/ChevronUp/Close.
Gaps -> nearest IconName: CIRCLE_NOTCH->Loader/LoaderCircle; WARNING->TriangleAlert (status/palette/console unset); LOCK/LOCK_OPEN->none (custom SVG); TREE_STRUCTURE->none exact; MAGNIFYING_GLASS->Search; SCROLL->none; IMAGE_SQUARE->none; PUZZLE_PIECE->none; PAINT_BUCKET->Palette (wired); SQUARES_FOUR->LayoutDashboard; HOUSE->none; EYE_SLASH->EyeOff (unused); X/X_CIRCLE->Close/CircleX (partial); CARET_*->Chevron* (partial); FILE/FOLDER wired; INFO wired.
No-lucide-match set requiring embedded phosphor SVGs: LOCK, LOCK_OPEN, PUZZLE_PIECE, HOUSE, TREE_STRUCTURE, SCROLL, IMAGE_SQUARE, SQUARES_FOUR.
