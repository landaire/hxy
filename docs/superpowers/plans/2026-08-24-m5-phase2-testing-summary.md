# hxy-gpui Phase 2: what needs manual testing

The unit suite is green (313 passed, 0 failed) and the release binary builds
offline. The items below are GUI behaviours the suite can only partially cover;
each needs a human at the running app. Launch with:

```
cargo run -p hxy-gpui --release -- <some-file> <another-file>
```

Open at least two files plus a couple of tool tabs (strings, entropy, console)
so the dock has real content to exercise.

## Landed this session

### 1. Hex cells: no gaps between differently-colored bytes
- Open a file whose bytes span several palette colors (e.g. a binary with text
  and null runs).
- Expect: adjacent hex cells of different colors touch with no seam/background
  line between them. The colored backgrounds bridge to the right edge of each
  cell.
- Regression watch: selection highlight still renders correctly over the
  bridged backgrounds (no color bleed past the selection range).

### 2. Minimap viewport indicator
- Open a file large enough to scroll. Scroll the hex view.
- Expect: the minimap shows a bracketed accent-colored band marking the slice
  of the file currently on screen, and the band tracks scrolling.

### 3. OS selection color (macOS)
- Select a byte range.
- Expect: the selection background matches the system selection color
  (System Settings > Appearance > Accent/Highlight color). Change the OS accent
  color and relaunch: the selection color follows it.
- Non-macOS: falls back to the theme selection color (no OS query).

### 4. Per-tab close button (fork)
- Hover/focus a tab.
- Expect: an X button on the tab; clicking it closes that tab only (not the
  active one) and does not also trigger a tab switch.
- Non-closable panels (if any) must not show the X.

### 5. Console (and tool panels) in the bottom dock
- Open the Console (palette "Console" or its action).
- Expect: it opens in the BOTTOM dock, below the center tab area, not as a
  center tab. Toggling it again hides/shows the bottom dock.
- Restore watch: quit and relaunch with a saved layout that had the console
  open; it should come back in the bottom dock without duplicating.

### 6. QuickOpen fuzzy tab switcher (Cmd+P) -- NEW
- Press Cmd+P.
- Expect: the palette opens in QuickOpen mode with the hint "Switch to open
  tab..." and lists every open tab (files AND tool tabs) across all dock
  regions, by name.
- Type part of a tab name; the list fuzzy-filters.
- Press Enter on a background tab: it comes to the front and takes focus.
- Press Cmd+P again while it is open: it toggles closed.
- Escape closes it (or, with `palette_escape_pops_to_parent` on, closes from
  QuickOpen since it has no parent mode).
- Covered by unit tests `quick_open_switches_to_a_background_tab` and
  `quick_open_toggles_closed_on_reinvoke`.

### 7. Dock migration to gpui-component 0.5.2 fork (underlies all of the above)
- General smoke: tabs render with titles and content, drag-drop docking still
  works (drag a tab to a side/edge to split), layout persists across relaunch,
  the inspector right-dock opens/closes, welcome panel appears on an empty
  workspace.
- This was the largest change; regressions here would show as missing tab
  chrome, blank panels, or lost layout on restart.

## Known deferred (not done this session)

These were scoped but not implemented; call them out if you test dock parity:

- **Palette Split/Merge/MoveTab verbs (#6):** egui exposes these as palette
  commands; gpui reaches the same result via native drag-drop docking. This is
  a deliberate, documented deviation, not a regression. No palette verbs added.
- **Nested-workspace cross-area drag guard (#7):** dragging a panel between the
  outer dock and a workspace-host's nested dock is not explicitly guarded in
  the fork. No crash has been reproduced, but this path is untested -- worth a
  targeted try (open a plugin-mount/workspace-host tab, drag a panel in/out of
  its nested dock) to confirm it does not corrupt layout state.
- **Floating / tear-off windows (#9):** multi-surface tear-off is an XL,
  separate milestone and was not started.

## Pre-existing issue (not from this work)

- The `elf.hexpat` template surfaces a garbled "Expected X got X" assert
  message. This is a template-engine bug that predates the dock work; it is not
  a Phase 2 regression.
