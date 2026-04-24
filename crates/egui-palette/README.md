# egui-palette

A Cmd+P-style command palette widget for [egui]. Modal popup, fuzzy
filter ([nucleo-matcher]), keyboard + mouse navigation.

## Install

```toml
[dependencies]
egui-palette = "0.1"
```

## Use

```rust
use egui_palette::{Entry, Outcome, State};

#[derive(Clone)]
enum Action {
    OpenFile,
    Close,
}

struct App {
    palette: State,
}

impl App {
    fn update(&mut self, ctx: &egui::Context) {
        // Toggle on Cmd+P / Ctrl+P.
        let shortcut = egui::KeyboardShortcut::new(
            egui::Modifiers::COMMAND,
            egui::Key::P,
        );
        if ctx.input_mut(|i| i.consume_shortcut(&shortcut)) {
            if self.palette.open {
                self.palette.close();
            } else {
                self.palette.open();
            }
        }

        let entries = vec![
            Entry::new("Open file", Action::OpenFile)
                .with_subtitle("Cmd+O")
                .with_icon("📁"),
            Entry::new("Close", Action::Close).with_icon("✖"),
        ];
        match egui_palette::show(ctx, &mut self.palette, &entries, "Search...") {
            Some(Outcome::Picked(Action::OpenFile)) => { /* ... */ }
            Some(Outcome::Picked(Action::Close)) => self.palette.close(),
            Some(Outcome::Closed) => self.palette.close(),
            None => {}
        }
    }
}
```

## Custom style

Everything tweakable lives on `Style`. Colour fields default to
`None` and follow `egui::Visuals`, so light / dark mode switches
track automatically.

```rust
use egui_palette::{Anchor, Style};

let style = Style::default()
    .anchored_at(Anchor::Center)
    .width_range(320.0, 480.0)
    .backdrop_fill(None);  // disable the darkened overlay
egui_palette::show_with_style(ctx, &mut state, &entries, "Search...", &style);
```

## License

MIT OR Apache-2.0.
