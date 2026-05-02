# egui_minimap

A scrollable minimap widget for [egui](https://github.com/emilk/egui).

The widget owns the viewport math (scroll fraction, indicator position, click/drag dispatch) and delegates the actual row painting to a user-supplied [`MinimapSource`]. That keeps the crate independent of the data being summarized: hex bytes, text lines, chat messages, audio waveforms, timelines, anything that can be expressed as "N rows of paintable strips".

## Quick example

```rust
use egui_minimap::{Minimap, MinimapSource, Viewport};

struct MyRows<'a> { colors: &'a [egui::Color32] }

impl MinimapSource for MyRows<'_> {
    fn row_count(&self) -> usize { self.colors.len() }
    fn paint_row(&self, painter: &egui::Painter, rect: egui::Rect, row: usize) {
        painter.rect_filled(rect, 0.0, self.colors[row]);
    }
}

fn ui(ui: &mut egui::Ui, rect: egui::Rect, colors: &[egui::Color32]) {
    let response = Minimap::new(&MyRows { colors })
        .scroll_id(ui.id().with("my_scroll"))
        .viewport(Viewport {
            total_rows: colors.len(),
            scroll_offset: 0.0,
            viewport_height: ui.available_height(),
            rows: ViewportRows::Uniform { row_height: 16.0 },
        })
        .show(ui, rect);

    if let Some(target) = response.scroll_target {
        // ...feed `target` into your scroll area on the next frame.
    }
}
```

## What the widget does

- Maps the source's full row range into the minimap rect, snapping each minimap row to a whole device pixel for flicker-free rendering during scroll.
- Renders a draggable viewport indicator. Clicking outside the indicator jumps; clicking inside grabs (no jump on press); dragging from inside scrolls relative; dragging from outside scrolls absolute.
- Stashes the requested scroll position in the egui ctx under the supplied `scroll_id` so the host can read it back next frame and feed it into a `ScrollArea`.
- Returns positioning info (window top row, shown rows, cell height) so the caller can paint custom overlays on top of the minimap (e.g., highlighting a hovered span).
