#!/usr/bin/env python3
"""Copy a built plugin (and optional sidecar manifest) into the
running hxy's plugin discovery directory.

Cross-platform: resolves the data dir via the same logic the host
uses (matches Rust's `dirs::data_dir()`):

  Linux:   $XDG_DATA_HOME/hxy or ~/.local/share/hxy
  macOS:   ~/Library/Application Support/hxy
  Windows: %APPDATA%\\hxy

Usage:
    deploy_plugin.py <crate-dir> <wasm-stem> [handler|template]

Examples:
    # Handler plugin -> $DATA/hxy/plugins/
    deploy_plugin.py plugins/xbox-neighborhood hxy_xbox_neighborhood handler

    # Template runtime -> $DATA/hxy/template-plugins/
    deploy_plugin.py plugins/bt-runtime hxy_bt_runtime template

The wasm artifact is read from
`<crate-dir>/target/wasm32-wasip2/release/<wasm-stem>.wasm`. If a
`<crate-dir>/<wasm-stem>.hxy.toml` sidecar exists it's copied
alongside.
"""

import os
import shutil
import sys
from pathlib import Path


def data_dir() -> Path:
    """Mirror `dirs::data_dir()` from the Rust dirs crate."""
    if sys.platform == "win32":
        appdata = os.environ.get("APPDATA")
        if not appdata:
            sys.exit("APPDATA not set; cannot resolve plugin install dir")
        return Path(appdata)
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Application Support"
    # Linux + other unixen: XDG_DATA_HOME, fallback ~/.local/share
    xdg = os.environ.get("XDG_DATA_HOME")
    if xdg:
        return Path(xdg)
    return Path.home() / ".local" / "share"


def main() -> int:
    if len(sys.argv) != 4 or sys.argv[3] not in ("handler", "template"):
        print(__doc__, file=sys.stderr)
        return 2

    crate_dir = Path(sys.argv[1])
    wasm_stem = sys.argv[2]
    kind = sys.argv[3]

    wasm_src = crate_dir / "target" / "wasm32-wasip2" / "release" / f"{wasm_stem}.wasm"
    if not wasm_src.exists():
        print(f"ERROR: missing wasm artifact: {wasm_src}", file=sys.stderr)
        print("       run `cargo build --target wasm32-wasip2 --release` first", file=sys.stderr)
        return 1

    sidecar_src = crate_dir / f"{wasm_stem}.hxy.toml"

    subdir = "plugins" if kind == "handler" else "template-plugins"
    dst_dir = data_dir() / "hxy" / subdir
    dst_dir.mkdir(parents=True, exist_ok=True)

    wasm_dst = dst_dir / wasm_src.name
    shutil.copy2(wasm_src, wasm_dst)
    print(f"deployed {wasm_src.name} ({wasm_dst.stat().st_size} bytes) -> {wasm_dst}")

    if sidecar_src.exists():
        sidecar_dst = dst_dir / sidecar_src.name
        shutil.copy2(sidecar_src, sidecar_dst)
        print(f"deployed {sidecar_src.name} -> {sidecar_dst}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
