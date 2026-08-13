# hxy

A hex editor built with Rust and [egui]. Desktop and web.

Reusable egui widget: [![hxy-view on crates.io](https://img.shields.io/crates/v/hxy-view.svg?label=hxy-view)](https://crates.io/crates/hxy-view) [![hxy-view on docs.rs](https://docs.rs/hxy-view/badge.svg)](https://docs.rs/hxy-view)

## Screenshots

VFS browser (zip):

![zip VFS](img/zip_vfs.png)

VFS browser (Xbox):

![Xbox VFS](img/xbox_vfs.png)

Loading a PNG from inside a zip:

![PNG loaded from zip](img/png_loaded_from_zip.png)

Expression calculator:

![expression calculator](img/expression_calculator.png)

Command palette:

![command palette](img/command_palette.png)

## Install

```
cargo install hxy
```

## Development

Nix is the supported development and CI environment. It pins the Rust toolchain,
system libraries, Cargo sources, and local dependency overlays.

```sh
nix develop
nix flake check
nix build .#hxy
nix develop --command buck2 build //:hxy
```

`//:hxy` is Buck's entry point to the hermetic Nix package. It stages only its
declared source files before Nix evaluates the flake; it does not traverse back
to the checkout. Reindeer also generates the native Rust graph in `BUCK` for
inspection and incremental Buck work, but that graph uses Buck's local system
toolchain and is not the reproducible release build.

## What's in the box

- File-backed hex view with selection, keyboard nav, drag-select, minimap
- Data inspector (integer widths, LEB128, float, time fields, RGBA/ARGB)
- 010 Editor Binary Template runtime (built in) -- or bring your own via WASM. 010 runtime does not have feature-parity, but can run some basic templates.
- ImHex pattern support
- VFS browser for archive formats (zip, etc.)
- IPC to open files from CLI in the existing window

## Status

too early to say.

Future plans:

- Refined plugin interface (it's day 1 and it's already a mess)
- Proper app bundling
- OS shell registration

## Goals

- Get to a good point so I can stop paying for a 010 Editor license
- Add process memory reading / raw disk reading
- Get working in web out of the box (not tested yet)
- Most components usable in library form so that people who need a hex view in an application can have one easily

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

[egui]: https://github.com/emilk/egui
