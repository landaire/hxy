# plugins

Sample WASM components that implement hxy's plugin interfaces. Each
is its own crate with its own `[workspace]` table so cargo-fuzz /
the main workspace don't pull them into their build graph.

## Template plugins

A **template plugin** is a WASM component that implements the
`hxy:vfs/template-runtime` world defined in `wit/world.wit`. The
host hands the plugin a byte source; the plugin hands back a tree of
named fields. There are two flavours; both land in
`$XDG_DATA_HOME/hxy/template-plugins/` (or the macOS / Windows
equivalent) and load at startup.

### Model 1 -- per-template WASM (preferred)

The WASM component *is* the template. A Rust / Zig / C++ author
hardcodes the format's layout in `execute()`, compiles to a
component, and ships a single `.wasm`. `parse("")` is a no-op. End
users don't need a DSL; they just drop the plugin in and it handles
their format.

See [`png-template/`](./png-template) -- parses PNG signature + chunks
+ IHDR fields in under 200 lines of Rust.

### Model 2 -- language runtime in WASM

One component ships an interpreter for a text DSL (010 Editor's
`.bt`, ImHex's `.hexpat`, etc.). End users keep writing templates in
the DSL; the runtime parses + executes them. Useful when there's an
existing ecosystem of text templates to reuse.

See [`bt-runtime/`](./bt-runtime) -- reference implementation of the
WIT world wrapping `hxy-010-lang`. The *same* interpreter also ships
linked-in natively as part of `hxy-app`; the WASM version is kept as
a dogfood target.

## VFS handler plugins

VFS handlers implement `hxy:vfs/plugin` -- they mount an archive-ish
byte source as a browseable virtual filesystem. Loaded from
`$XDG_DATA_HOME/hxy/plugins/`. See [`passthrough/`](./passthrough)
for a trivial example that exposes the whole source as a single
`/data.bin`.

## Build workflow

```sh
cd plugins/<name>
cargo build --target wasm32-unknown-unknown --release
wasm-tools component new \
    target/wasm32-unknown-unknown/release/hxy_<name>.wasm \
    -o target/<name>.component.wasm
# Then drop `<name>.component.wasm` into the appropriate data dir.
```

Requires the `wasm32-unknown-unknown` target (`rustup target add`)
and `wasm-tools` (`cargo install wasm-tools`).
