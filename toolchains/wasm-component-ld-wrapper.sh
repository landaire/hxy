#!/usr/bin/env bash
# Link wasm plugin components with rustc's own wasm-component-ld.
#
# buck's "wasm" cxx linker type prepends clang- and rust-lld-style flags
# (-fuse-ld=lld, -flavor wasm) that wasm-component-ld does not accept;
# rustc itself passes neither when it links a wasip2 component. Strip
# them and forward the rest. The real linker store path is written next
# to this script by the nix dev-shell hook (see flake.nix).
set -euo pipefail

ld="$(cat "$(dirname "$0")/.wasm-component-ld-path")"

args=()
skip=0
for a in "$@"; do
    if [ "$skip" = 1 ]; then
        skip=0
        continue
    fi
    case "$a" in
        -fuse-ld=*) ;;
        -flavor) skip=1 ;;
        *) args+=("$a") ;;
    esac
done

exec "$ld" "${args[@]}"
