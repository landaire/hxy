# hxy

A hex editor built with Rust and [egui]. Desktop and web.

## Install

```
cargo install hxy-app
```

Binary is named `hxy`.

## What's in the box

- File-backed hex view with selection, keyboard nav, drag-select, minimap
- Data inspector (integer widths, LEB128, float, time fields, RGBA/ARGB)
- 010 Editor Binary Template runtime (built in) — or bring your own via WASM
- VFS browser for archive formats (zip, etc.)

## Status

too early to say

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

[egui]: https://github.com/emilk/egui
