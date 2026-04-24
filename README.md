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
- 010 Editor Binary Template runtime (built in) — or bring your own via WASM. 010 runtime does not have feature-parity, but can run some basic templates.
- VFS browser for archive formats (zip, etc.)

## Status

too early to say.

Future plans:

- ImHex pattern runtime
- Write support
- Refined plugin interface (it's day 1 and it's already a mess)
- Proper app bundling
- OS shell registration

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

[egui]: https://github.com/emilk/egui
