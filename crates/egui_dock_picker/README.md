# egui_dock_picker

A hint-style "press a key, jump a pane" picker overlay for [egui_dock](https://github.com/Adanos020/egui_dock).

While active, every dock leaf gets a big bold letter painted dead-center over its area. Pressing that letter resolves a [`NodePath`](egui_dock::NodePath) that the host can then use for any operation: focus the pane, move a tab, merge two leaves, close a pane, anything else.

This crate doesn't know what your operation is. The host stages a [`PanePickConfig`], calls [`tick`] each frame after the dock has rendered, and gets back a [`TickOutcome::Picked { source, target }`] once the user presses a target letter. Mapping that outcome onto a concrete dock mutation is the host's job.

## Quick example

```rust
use std::collections::BTreeMap;
use egui_dock_picker::{tick, PanePickConfig, TickOutcome};

struct App {
    dock: egui_dock::DockState<MyTab>,
    pending_pick: Option<egui_dock::NodePath>,
    assignments: BTreeMap<u64, char>,
}

fn drive_picker(app: &mut App, ctx: &egui::Context) {
    let Some(source) = app.pending_pick else { return };

    let outcome = tick(
        ctx,
        &app.dock,
        PanePickConfig {
            source: Some(source),
            source_badge_label: Some("MOVE FROM"),
            target_whitelist: None,
        },
        &mut app.assignments,
    );

    match outcome {
        TickOutcome::Continue => {}
        TickOutcome::Cancel => app.pending_pick = None,
        TickOutcome::Picked { source: Some(src), target } => {
            // Apply the operation against `src` and `target`.
            app.pending_pick = None;
        }
        TickOutcome::Picked { source: None, target: _ } => {
            // Sourceless pick (host configured source: None).
            app.pending_pick = None;
        }
    }
}
```

## What the picker does

- Hashes each leaf's tab loadout into a stable identity, so a leaf keeps its assigned letter across pick sessions even as the dock around it changes.
- Lays a transparent backdrop over the entire viewport that swallows clicks during the pick, so the dock underneath doesn't react.
- Optionally paints a "FROM" badge over the source leaf when the source is configured.
- Cancels cleanly on Escape, on no-targets, or when called with `Picked` resolution.

The host owns the `BTreeMap<u64, char>` letter assignments map and persists it across frames; entries are evicted automatically when their leaves go away.
