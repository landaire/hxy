# GPUI API notes for the hex-editor widget (M1)

Researched 2026-07-24 from real source checkouts. Citations use these pins:

- `gpui-component` = github.com/longbridge/gpui-component @ `bc174a7ec4534b2a4174fddde314b38d30d69093` (main)
- `zed` = github.com/zed-industries/zed @ `d23aaeebeafd509ff089cc822edd64ccce585181` (main; the gpui crate lives in `crates/gpui`)
- `rusthex` = github.com/suma/rusthex @ `4469f9e7b997382ffbea402b2f1dcbce885df0b1` (prior-art hex editor on gpui)

## 1. Dependency pin facts

Two viable routes:

Route A, crates.io (what rusthex ships with; `rusthex:Cargo.toml`):

    gpui = "0.2.2"            # registry, self-contained, has Application::new()
    gpui-component = "0.5.1"  # 0.5.2 is current per gpui-component workspace Cargo.toml

On this route there is NO separate `gpui_platform` crate (`rusthex:Cargo.lock` has
no `gpui_platform` entry) and you bootstrap with `gpui::Application::new()`
(`rusthex:src/main.rs:676`).

Route B, git (what gpui-component main uses; `gpui-component:Cargo.toml` lines 35-38):

    gpui = { git = "https://github.com/zed-industries/zed" }
    gpui_platform = { git = "https://github.com/zed-industries/zed", features = ["font-kit", "x11", "wayland", "runtime_shaders"] }
    gpui_macros = { git = "https://github.com/zed-industries/zed" }

No `rev` in the Cargo.toml; gpui-component's Cargo.lock pins zed at
`1a246efd7e1b83ab568ec5e3e6c1a43a42e1abba` (gpui version string 0.2.2).
On the git route `gpui_platform` IS a separate crate (`zed:crates/gpui_platform`)
and git-HEAD gpui has no `Application::new()`; you must use
`gpui_platform::application()` which calls `Application::with_platform(current_platform(false))`
(`zed:crates/gpui_platform/src/gpui_platform.rs:13`).

Toolchain: gpui-component workspace is `edition = "2024"` (`gpui-component:Cargo.toml:27`);
no `rust-version` key in either workspace. rusthex is also edition 2024.
Our Rust 1.92 / edition 2024 workspace is compatible with both routes.

gpui-component also sets `useless_conversion = "allow"` at workspace lint level and
builds gpui at `opt-level = 3` even in dev (`gpui-component:Cargo.toml` profile section);
copy the opt-level trick or debug builds will paint slowly.

## 2. App bootstrap

Modern gpui-component pattern (`gpui-component:examples/hello_world/src/main.rs`, verbatim):

    fn main() {
        gpui_platform::application().run(move |cx| {
            // This must be called before using any GPUI Component features.
            gpui_component::init(cx);
            cx.spawn(async move |cx| {
                cx.open_window(WindowOptions::default(), |window, cx| {
                    let view = cx.new(|_| Example);
                    // This first level on the window, should be a Root.
                    cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
                })
                .expect("Failed to open window");
            })
            .detach();
        });
    }

Key signatures:

- `Application::run<F>(self, on_finish_launching: F) where F: 'static + FnOnce(&mut App)`
  (`zed:crates/gpui/src/app.rs:225`). Builder methods: `.with_assets(...)`,
  `.with_http_client(...)`, `.with_quit_mode(...)`.
- `App::open_window<V: 'static + Render>(&mut self, options: WindowOptions, build_root_view: impl FnOnce(&mut Window, &mut App) -> Entity<V>) -> anyhow::Result<WindowHandle<V>>`
  (`zed:crates/gpui/src/app.rs:1217`).
- `WindowOptions` fields include `window_bounds: Option<WindowBounds>`, `titlebar: Option<TitlebarOptions>`, `focus: bool`, `show: bool`, `kind: WindowKind`, `is_movable: bool` (`zed:crates/gpui/src/platform.rs:1749`).
- `gpui_component::init(cx)` runs `theme::init`, `input::init`, `menu::init`, etc.
  (`gpui-component:crates/ui/src/lib.rs:109`). Returns `()`, not Result.
- `Root::new(view: impl Into<AnyView>, window: &mut Window, cx: &mut Context<Self>) -> Root`
  (`gpui-component:crates/ui/src/root.rs:96`). Root hosts dialogs/popovers/notifications;
  it must be the window's top-level view.

## 3. Rendering a monospace hex grid

### canvas element

`zed:crates/gpui/src/elements/canvas.rs:10`:

    pub fn canvas<T>(
        prepaint: impl 'static + FnOnce(Bounds<Pixels>, &mut Window, &mut App) -> T,
        paint: impl 'static + FnOnce(Bounds<Pixels>, T, &mut Window, &mut App),
    ) -> Canvas<T>

Both closures are FnOnce and are recreated every render. `prepaint` returns arbitrary
state `T` handed to `paint`. Canvas has a `StyleRefinement` so you size it with normal
styled methods (`.size_full()` etc.); it does NOT size itself to content.

### Shaping and painting text runs

`Window::text_system()` returns `&WindowTextSystem`.

- `WindowTextSystem::shape_line(&self, text: SharedString, font_size: Pixels, runs: &[TextRun], force_width: Option<Pixels>) -> ShapedLine`
  (`zed:crates/gpui/src/text_system.rs:397`). Panics in debug if text contains `\n`.
  There is also `shape_line_by_hash(...)` (line 448) for caching by content hash.
- `TextRun { len: usize /* utf8 bytes */, font: Font, color: Hsla, background_color: Option<Hsla>, underline: Option<UnderlineStyle>, strikethrough: Option<StrikethroughStyle> }`
  (`zed:crates/gpui/src/text_system.rs:987`).
- `ShapedLine::paint(&self, origin: Point<Pixels>, line_height: Pixels, align: TextAlign, align_width: Option<Pixels>, window: &mut Window, cx: &mut App) -> Result<()>`
  and `paint_background(...)` with the same shape (`zed:crates/gpui/src/text_system/line.rs:83,108`).
  `ShapedLine::width() -> Pixels` (line 63).
- Hit-testing within a shaped line: `LineLayout::index_for_x(x) -> Option<usize>`,
  `closest_index_for_x(x) -> usize`, `x_for_index(i) -> Pixels`
  (`zed:crates/gpui/src/text_system/line_layout.rs:58,75,105`). ShapedLine derefs to LineLayout.
- Monospace cell width: `TextSystem::em_advance(font_id, font_size) -> Result<Pixels>`
  and `advance(font_id, font_size, ch)` (`zed:crates/gpui/src/text_system.rs:233,195`).
  rusthex caches it: `self.cached_char_width = window.text_system().em_advance(font_id, font_size)`
  (`rusthex:src/render.rs:2881`).
- `Window::line_height() -> Pixels` (`zed:crates/gpui/src/window.rs:2649`).

### Quads for cell backgrounds

- `Window::paint_quad(&mut self, quad: PaintQuad)` (`zed:crates/gpui/src/window.rs:3954`).
- `fill(bounds: impl Into<Bounds<Pixels>>, background: impl Into<Background>) -> PaintQuad`
  (`zed:crates/gpui/src/window.rs:6583`); `outline(bounds, border_color, border_style)` (line 6595).

### How Zed's terminal paints its grid (the model to copy)

`zed:crates/terminal_view/src/terminal_element.rs`: background rects via
`window.paint_quad(fill(Bounds::new(position, size), self.color))` (line 215), text via
`window.text_system().shape_line(...)` (lines 162, 1222, 1447) then `ShapedLine::paint`.
It is a custom `Element` impl, but `canvas()` gives the same paint API without the
Element boilerplate.

### How rusthex does it (works, but heavier)

rusthex does NOT use canvas or shape_line for the grid. It builds a div per row and a
div per byte cell inside a manually virtualized scroll container: outer div with
`.overflow_y_scroll().track_scroll(&self.tab().scroll_handle)`, a top spacer div of
`top_spacer_height`, then `.children((render_start..render_end).map(|row| ...))`, each
row div with `.id(("hex-row", row))` and per-byte child divs with
`.id(("hex-byte", ...))` (`rusthex:src/render.rs:662-900`). It offsets the cost with a
`RenderCache` keyed by row index plus modified-offset invalidation
(`rusthex:src/render_cache.rs`). This is O(visible cells) retained elements per frame;
fine at 16x40 cells, but canvas + shape_line (one ShapedLine per row, or per style run)
is cheaper and gives exact pixel control. Recommendation: canvas for the grid,
divs for chrome.

### uniform_list (row virtualization alternative)

`zed:crates/gpui/src/elements/uniform_list.rs:22`:

    pub fn uniform_list<R: IntoElement>(
        id: impl Into<ElementId>,
        item_count: usize,
        f: impl 'static + Fn(Range<usize>, &mut Window, &mut App) -> Vec<R>,
    ) -> UniformList

All rows must be uniform height (it measures the first item). Scroll state via
`UniformListScrollHandle` and `.track_scroll(&handle)` (lines 80, 683). Good fit if each
hex row is rendered as one element; the closure only gets the visible index range.

### Geometry basics

`px(f32) -> Pixels` (logical pixels, f32 newtype, `f32::from(pixels)` works).
`Point<T> { x, y }`, `Size<T> { width, height }`,
`Bounds<T> { origin: Point<T>, size: Size<T> }` (`zed:crates/gpui/src/geometry.rs:723`).
`Bounds::new(origin, size)`, `bounds.contains(&point)` available.

## 4. Input

### Keyboard

- `KeyDownEvent { keystroke: Keystroke, is_held: bool, prefer_character_input: bool }`
  (`zed:crates/gpui/src/interactive.rs:25`).
- `Keystroke { modifiers: Modifiers, key: String, key_char: Option<String> }`
  (`zed:crates/gpui/src/platform/keystroke.rs:18`). `key` is the printed key cap
  ("s", "enter", "escape", "up", "tab"); `key_char` is the char that would be typed
  ("s", option-s => "SS char", cmd-s => None).
- `Modifiers { control, alt, shift, platform, function: bool }`; `platform` is cmd on
  macOS, win key on Windows, super on Linux (`zed:crates/gpui/src/platform/keystroke.rs:448`).
- Focus + key routing: create a `FocusHandle` (via `cx.focus_handle()`), put
  `.track_focus(&handle)` on the div (`zed:crates/gpui/src/elements/div.rs:722`), then
  `.on_key_down(listener)` where
  `listener: impl Fn(&KeyDownEvent, &mut Window, &mut App) + 'static`
  (`zed:crates/gpui/src/elements/div.rs:469`). Key events dispatch along the focus path.
  `FocusHandle::focus(&self, window, cx)` and `is_focused(&self, window)` at
  `zed:crates/gpui/src/window.rs:541,546`.
- Use `cx.listener(...)` to reach entity state from handlers:
  `Context<T>::listener(f: impl Fn(&mut T, &E, &mut Window, &mut Context<T>)) -> impl Fn(&E, &mut Window, &mut App)`
  (`zed:crates/gpui/src/app/context.rs:252`).
- Actions and bindings: `actions!(namespace, [Copy, Paste, ...])`, `cx.bind_keys([KeyBinding::new("cmd-s", actions::Save, None), ...])`, elements get `.on_action(...)` and `.key_context("HexEditor")` scoping. rusthex does exactly this
  (`rusthex:src/actions.rs:5`, `rusthex:src/main.rs:688-697`). Keystroke string format:
  `"cmd-shift-s"`, `"ctrl-n"`, dash-separated modifiers then key.
- IME/text input: for hex/ASCII overwrite typing, `KeyDownEvent.keystroke.key_char` is
  sufficient (this is how you get shifted/altgr chars without IME). Full
  `EntityInputHandler` (`zed:crates/gpui/src/input.rs:10`, methods `text_for_range`,
  `selected_text_range`, `replace_text_in_range`, ...) is only needed for real IME
  composition; gpui-component's TextInput registers it during element paint via
  `window.handle_input(&focus_handle, ElementInputHandler::new(bounds, state), cx)`
  (`gpui-component:crates/ui/src/input/element.rs:1930`, `zed:crates/gpui/src/window.rs:4625`).
  Skip for M1; ASCII column editing works from key_char.

### Mouse

- `MouseDownEvent { button: MouseButton, position: Point<Pixels>, modifiers, click_count: usize, first_mouse: bool }` (`zed:crates/gpui/src/interactive.rs:139`).
- `MouseUpEvent { button, position, modifiers, click_count }` (line 176).
- `MouseMoveEvent { position, pressed_button: Option<MouseButton>, modifiers }` (line 485).
- On divs: `.on_mouse_down(MouseButton::Left, listener)`, `.on_mouse_up`, `.on_mouse_move`,
  `.on_mouse_down_out` (`zed:crates/gpui/src/elements/div.rs:122-299`). Div handlers are
  hit-tested against the div's bounds automatically during bubble phase.
- Inside a canvas paint closure you instead register raw listeners:
  `window.on_mouse_event(move |ev: &MouseMoveEvent, phase: DispatchPhase, window, cx| ...)`
  (`zed:crates/gpui/src/window.rs:4646`). These see ALL window mouse events for the next
  frame; you do your own `bounds.contains(&ev.position)` check and usually act on
  `phase == DispatchPhase::Bubble`. Listeners are per-frame; re-register every paint.
- Drag-select pattern (terminal/editor style): record anchor byte offset on mouse-down
  into entity state; on mouse-move with `pressed_button == Some(MouseButton::Left)`
  extend selection and `cx.notify()`; clear drag flag on mouse-up. `click_count` gives
  double/triple click for word/row select.

### Scroll

- `ScrollWheelEvent { position: Point<Pixels>, delta: ScrollDelta, modifiers: Modifiers, touch_phase: TouchPhase }` (`zed:crates/gpui/src/interactive.rs:513`).
- `ScrollDelta::Pixels(Point<Pixels>) | ScrollDelta::Lines(Point<f32>)` (line 545);
  convert lines using your line height.
- Options: (a) manage a `ScrollOffset` field yourself in the entity and handle
  `.on_scroll_wheel(...)` (best for canvas grid; you own clamping and row math), or
  (b) `ScrollHandle::new()` + `.overflow_y_scroll().track_scroll(&handle)` on a div;
  `ScrollHandle::offset() -> Point<Pixels>` / `set_offset(Point<Pixels>)` / `max_offset()`
  (`zed:crates/gpui/src/elements/div.rs:3923-4074`). rusthex uses (b) plus spacer divs.

## 5. Clipboard

- `App::write_to_clipboard(&self, item: ClipboardItem)` and
  `read_from_clipboard(&self) -> Option<ClipboardItem>` (`zed:crates/gpui/src/app.rs:1349,1334`).
- `ClipboardItem { entries: Vec<ClipboardEntry> }`; construct with
  `ClipboardItem::new_string(text: String)` or `new_string_with_metadata(text, metadata)`
  (`zed:crates/gpui/src/platform.rs:2221,2239`). Read text back with `item.text()`.

## 6. Theme and appearance

- `WindowAppearance { Light, VibrantLight, Dark, VibrantDark }`
  (`zed:crates/gpui/src/platform.rs:1992`). Query via `window.appearance()`
  (`zed:crates/gpui/src/window.rs:2455`) or `cx.window_appearance()` (`zed:crates/gpui/src/app.rs:1324`).
- Observe changes: `window.observe_window_appearance(impl FnMut(&mut Window, &mut App)) -> Subscription`
  (`zed:crates/gpui/src/window.rs:1942`).
- gpui-component: `Theme::sync_system_appearance(window: Option<&mut Window>, cx: &mut App)`
  (`gpui-component:crates/ui/src/theme/mod.rs:142`; prefers `window.appearance()` because
  `cx.window_appearance()` errors on Linux, see comment there). Also
  `Theme::sync_scrollbar_appearance(cx)` (line 154). `theme::init` is called by
  `gpui_component::init` and does initial sync.
- Reading colors in render: `use gpui_component::ActiveTheme;` then `cx.theme()` returns
  `&Theme` (trait `ActiveTheme { fn theme(&self) -> &Theme }`,
  `gpui-component:crates/ui/src/theme/mod.rs:32`; implemented for `App`, so it works on
  any `Context<T>` via deref). Fields like `cx.theme().background`, `.muted_foreground`.

## 7. Testing

- `#[gpui::test]` (from gpui_macros) on `fn test_x(cx: &mut TestAppContext)`; supports
  async fns too. Headless platform, deterministic executor.
- `TestAppContext::add_empty_window(&mut self) -> &mut VisualTestContext`
  (`zed:crates/gpui/src/app/test_context.rs:267`), or `add_window_view` (line 288) to
  mount a view.
- `VisualTestContext` (line 738): `simulate_keystrokes("cmd-a escape x")` (line 794),
  `simulate_click(position, modifiers)` (line 850), `simulate_event::<E: InputEvent>(event)`
  (line 925). It derefs to TestAppContext; `cx.update(|window, cx| ...)` gives Window access.
- Real example, `gpui-component:crates/ui/src/combobox.rs:1085`:

      #[gpui::test]
      fn test_combo_box_builder(cx: &mut TestAppContext) {
          cx.update(crate::init);
          let cx = cx.add_empty_window();
          cx.update(|window, cx| {
              let state = cx.new(|cx| ComboboxState::new(items, vec![], window, cx));
              ...
          });
      }

  Pattern for us: `cx.update(gpui_component::init)` (or our own init), mount the hex view
  with `add_window_view`, drive with `simulate_keystrokes` / `simulate_event(MouseDownEvent {...})`,
  assert on entity state via `entity.read(cx)`.

## 8. gpui-component shell pieces

- `TitleBar` exists: `TitleBar::new().child(...)` plus
  `WindowOptions { titlebar: Some(TitleBar::title_bar_options()), .. }`
  (`gpui-component:crates/ui/src/title_bar.rs:24`, usage in
  `gpui-component:examples/window_title/src/main.rs`).
- `h_flex() -> Div` / `v_flex() -> Div` presets (`gpui-component:crates/ui/src/styled.rs:10,16`);
  also available as `.v_flex()` style methods on div.
- `Label` (`gpui-component:crates/ui/src/label.rs`), `Button::new("id").primary().label("...").on_click(|ev, window, cx| ...)`
  (hello_world example). Button ids are `ElementId`s; any `impl Into<ElementId>`.
- Single-view app wiring = section 2 snippet. With assets:
  `gpui_platform::application().with_assets(gpui_component_assets::Assets)`
  (`gpui-component:examples/window_title/src/main.rs`), needed for gpui-component icons.

## 9. Gotchas coming from egui

- Retained, not immediate: state lives in `Entity<T>` structs created with
  `cx.new(|cx| ...)`. `Render::render(&mut self, window, cx)` rebuilds the element tree
  only when the entity is notified. Nothing repaints per-frame by default.
- `cx.notify()` is the redraw trigger. Mutating entity state without notify leaves the
  old frame on screen. `entity.update(cx, |state, cx| { ...; cx.notify(); })` from
  outside, or `cx.notify()` inside a `Context<T>` handler.
- Event closures must not capture `&mut self`; use `cx.listener(...)` which weakly
  captures the entity and re-borrows it on dispatch (`zed:crates/gpui/src/app/context.rs:252`).
- Element lifecycle is request_layout -> prepaint -> paint each frame. `canvas()`
  closures are FnOnce and rebuilt every render; capture cheap clones (theme colors,
  offsets, an `Entity<T>` handle) into them. Layout (taffy) decides the canvas bounds;
  your paint closure receives the final `Bounds<Pixels>`.
- `window.on_mouse_event` / `window.handle_input` registrations made during paint last
  one frame only; re-register in every paint (this is normal, terminal_element does it).
- `SharedString` is the string currency (cheap clone, Arc-backed); build per-row strings
  once and cache if profiling shows shaping cost; `shape_line_by_hash` exists for that.
- Debug builds are slow: set `[profile.dev.package] gpui = { opt-level = 3 }` like
  gpui-component does.
- Scroll offsets in gpui go negative as content scrolls up (offset is the translation of
  content relative to the container); clamp against `max_offset()` when using ScrollHandle.
- ASCII-only key facts: `keystroke.key` is layout-normalized ("q" even on non-Latin
  layouts); the typed character is `key_char`. Match editing input on `key_char`,
  shortcuts on `key` + `modifiers` (or better, actions + keymap).

## crates.io corrections (0.2.2 / 0.5.1)

The git route (Route B above) was abandoned: `gpui-component` (any rev)
requests `gpui`/`gpui_macros`/`gpui_web`/`reqwest_client` from
`zed-industries/zed` without a `rev`, and Cargo cannot `[patch]` a git
source onto a different revision of the *same* repository URL
(`rust-lang/cargo#10756`, `rust-lang/cargo#7670`, both open/unresolved --
confirmed empirically: every patch variant errors "points to the same
source, but patches must point to different sources"). M1 now pins the
crates.io route instead: `gpui = "=0.2.2"`, `gpui-component = "=0.5.1"`
(not `0.5.2` -- see below). Corrections to this document's git-route
research, verified against `~/.local/share/cargo/registry/src/.../gpui-0.2.2`
and `.../gpui-component-0.5.1`:

- **`gpui-component` version**: `0.5.2` is only the git workspace's
  in-progress `Cargo.toml` version string; crates.io's published index
  tops out at `0.5.1` (`0.5.1-preview0` before it). Use `0.5.1`, which is
  also what rusthex actually depends on.
- **Metal shader build requirement**: `gpui` 0.2.2's `build.rs`
  (`gpui-0.2.2/build.rs:66-77`) ahead-of-time-compiles
  `src/platform/mac/shaders.metal` via `xcrun -sdk macosx metal`, which
  ships only with full Xcode.app, not Xcode Command Line Tools. On a
  CLT-only macOS machine this fails with
  `xcrun: error: unable to find utility "metal", not a developer tool or
  in PATH`. Enable the `runtime_shaders` feature on `gpui`
  (`gpui = { version = "=0.2.2", features = ["runtime_shaders"] }`) --
  `build.rs` then calls `emit_stitched_shaders` instead of
  `compile_metal_shaders` (`gpui-0.2.2/build.rs:70-73`), stitching/compiling
  shaders at runtime and needing no `metal` binary at build time. This is
  the same feature gpui-component's own git-route Cargo.toml enables on
  `gpui_platform` (section 1 above), just relevant here because
  crates.io's `gpui` doesn't default it on.
- **Bootstrap**: as documented above (section 1/2), `gpui::Application::new()`
  works directly on 0.2.2 (no `gpui_platform` crate on this route) and
  `gpui_component::init(cx)` / `Theme::sync_system_appearance` behave as
  described.
- **`Root::new(...)` does not have a `.bg()` builder method** -- unlike a
  `div()`, `Root` (`gpui-component-0.5.1/src/root.rs:240`) is a plain
  struct, not `Styled`. Its own `Render` impl already applies
  `.bg(cx.theme().background)` to its root `div()`
  (`gpui-component-0.5.1/src/root.rs:396-413`), so the window's
  `build_root_view` closure is just
  `cx.new(|cx| Root::new(workspace, window, cx))` -- no `.bg(...)` call,
  and no need to set a background on `Root` itself.

### Task 3 corrections (canvas / text / theme at 0.2.2)

Verified against `gpui-0.2.2` and `gpui-component-0.5.1` in the local
crates.io registry while implementing the hex grid paint pass.

- **`ShapedLine::paint` has NO `align` / `align_width` params at 0.2.2.**
  The git-HEAD signature this doc cited (section 3) was
  `paint(origin, line_height, align, align_width, window, cx)`; the
  published 0.2.2 signature is
  `paint(&self, origin: Point<Pixels>, line_height: Pixels, window: &mut Window, cx: &mut App) -> Result<()>`
  (`gpui-0.2.2/src/text_system/line.rs:67`). `paint_background` matches.
  Alignment is hardcoded to `TextAlign::default()` internally.
- **`resolve_font` / `em_advance` live on `TextSystem`, not
  `WindowTextSystem`,** but `WindowTextSystem` `#[deref]`s to
  `Arc<TextSystem>` (`gpui-0.2.2/src/text_system.rs:333`), so
  `window.text_system().resolve_font(&font)` and
  `.em_advance(font_id, size)` both resolve through Deref. `resolve_font`
  PANICS if neither the font nor any fallback resolves; `em_advance`
  returns `Result`.
- **`outline(bounds, border_color, border_style)`** takes a
  `BorderStyle` (`BorderStyle::Solid` / `Dashed`,
  `gpui-0.2.2/src/scene.rs:508`) as its third arg and produces a 1px
  border; `fill(bounds, background)` produces a filled quad. Free fn
  `bounds(origin, size)` builds a `Bounds`. `Hsla::opacity(factor)`
  scales alpha (`gpui-0.2.2/src/color.rs:549`).
- **Theme mono tokens**: `gpui_component::Theme` (0.5.1) exposes
  `mono_font_family: SharedString` and `mono_font_size: Pixels` directly
  (`gpui-component-0.5.1/src/theme/mod.rs:59,61`); on macOS the family
  defaults to Menlo. `Theme` `Deref`s to `ThemeColor`, whose fields
  include `background`, `foreground`, `muted_foreground`, `accent`,
  `accent_foreground`, `selection`, `border`
  (`.../theme/theme_color.rs`). No separate "mono color" token; the byte
  classes map onto `foreground` / `muted_foreground` / `accent_foreground`.
- **`Entity::update` on `&mut App` returns `R` directly** (not a
  `Result`): `App`'s `AppContext::Result<R> = R`
  (`gpui-0.2.2/src/app/entity_map.rs:430`). This is how the canvas paint
  closure writes `FrameInfo` back onto the entity mid-frame; a captured
  strong `Entity<HexPane>` handle is safe because the canvas element (and
  its `FnOnce` closures) is dropped at frame end.
- **`#[gpui::test]` needs `gpui`'s `test-support` feature.**
  `TestAppContext` / `run_test` are gated behind it
  (`gpui-0.2.2/Cargo.toml [features] test-support`); add
  `gpui = { version = "=0.2.2", features = ["runtime_shaders", "test-support"] }`
  as a dev-dependency. `TestAppContext::add_window(build) ->
  WindowHandle<V>`; drive the entity via `window.update(cx, |view, window,
  cx| ...)` (returns `Result`) and read via `entity.read_with(cx, |v, _|
  ...)` (returns the value directly). Rendering touches `cx.theme()`, so
  call `cx.update(gpui_component::init)` before `add_window` or the theme
  global is missing and render panics.
