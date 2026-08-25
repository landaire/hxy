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

### 8. Pane split / merge / move-tab palette verbs (#6) -- NEW

- Open the palette (Cmd+Shift+P) with a file open. Near the bottom are:
  Split pane right/left/up/down, Move tab right/left/up/down, Merge pane
  with right/left/up/down.
- **Split**: adds an empty pane on the chosen side and focuses it. Opening a
  file then fills it.
- **Move tab**: with two side-by-side panes, moves the active tab into the
  neighbor pane. At the layout edge (no neighbor that way) it does nothing.
- **Merge**: folds the active pane's tabs into the neighbor pane and removes
  the emptied pane. Edge = no-op.
- Note: "active pane" is the first center tab group in tree order (matches the
  rest of the app's active-pane rule), which may differ from the visually
  focused split -- verify the verbs act on the pane you expect in a multi-split
  layout.

### 9. Cross-window drag guard (#7) -- NEW (fork)

- Open a plugin-mount / workspace-host tab (it hosts a nested dock). Drag a tab
  from the outer dock into the nested dock, and vice versa.
- Expect: the drop is rejected and the tab stays where it was -- no ghost or
  duplicated tab. (Same-dock drag-docking is unaffected.)

### 10. Tear-off windows (#9) -- NEW

- With a file tab active, run the palette command "Move tab to new window".
- Expect: a second OS window opens showing that tab's hex view; the tab leaves
  the main window.
- Edit bytes in the torn-off window, then close that window: the tab returns to
  the main window with its unsaved edits intact (the panel entity is preserved
  across the move, so nothing is lost).
- Known limitations of the torn-off window: it is a bare hex view -- no
  inspector / status bar / minimap, and the file is not watched for external
  changes while floating (watch resumes on reclaim). The main window's
  inspector/status do not reflect the torn tab while it floats.

## Pre-existing issue (not from this work)

- The `elf.hexpat` template surfaces a garbled "Expected X got X" assert
  message. This is a template-engine bug that predates the dock work; it is not
  a Phase 2 regression.
