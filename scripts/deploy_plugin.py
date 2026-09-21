#!/usr/bin/env python3
"""Build a plugin component with buck2 and copy it (plus its sidecar
manifest) into the running hxy's plugin discovery directory.

Cross-platform: resolves the data dir via the same logic the host
uses (matches Rust's `dirs::data_dir()`):

  Linux:   $XDG_DATA_HOME/hxy or ~/.local/share/hxy
  macOS:   ~/Library/Application Support/hxy
  Windows: %APPDATA%\\hxy

Usage:
    deploy_plugin.py <buck-target> <sidecar> [handler|template]

Examples:
    # Handler plugin -> $DATA/hxy/plugins/
    deploy_plugin.py //:xbox-neighborhood-plugin \\
        plugins/xbox-neighborhood/hxy_xbox_neighborhood.hxy.toml handler

Run inside `nix develop` so buck2 and the wasm toolchain are on PATH.
The plugin crates are workspace members built for wasm32-wasip2 by
buck2 (see //platforms:wasm32-wasip2 and the `*-plugin` genrules in the
generated BUCK); the target's output is the component to deploy. The
sidecar `.hxy.toml` is copied verbatim when the path exists.
"""

import os
import shutil
import subprocess
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


def build(target: str) -> Path | None:
    """Build the buck2 target and return the path to its wasm output."""
    print(f"building {target} with buck2")
    result = subprocess.run(
        ["buck2", "build", target, "--show-output"],
        capture_output=True,
        text=True,
    )
    sys.stderr.write(result.stderr)
    if result.returncode != 0:
        return None
    # `--show-output` prints "<target> <relative-path>" lines; the wasm is
    # the one output of a `*-plugin` genrule.
    for line in result.stdout.splitlines():
        parts = line.split()
        if parts and parts[-1].endswith(".wasm"):
            return Path(parts[-1])
    return None


def main() -> int:
    if len(sys.argv) != 4 or sys.argv[3] not in ("handler", "template"):
        print(__doc__, file=sys.stderr)
        return 2

    target = sys.argv[1]
    sidecar_src = Path(sys.argv[2])
    kind = sys.argv[3]

    wasm_src = build(target)
    if wasm_src is None or not wasm_src.exists():
        print(f"ERROR: buck2 build did not produce a wasm output for {target}", file=sys.stderr)
        return 1

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
    else:
        print(f"WARNING: sidecar manifest not found: {sidecar_src}", file=sys.stderr)

    return 0


if __name__ == "__main__":
    sys.exit(main())
