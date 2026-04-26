#!/usr/bin/env bash
# Clone (or fast-forward) the upstream ImHex-Patterns corpus into the
# repo-local `.imhex-patterns/` directory. The corpus is GPL-licensed
# and is *not* redistributed with hxy -- we link against it at
# runtime to resolve `import` / `#include` references and to provide
# the bundled `std/` library. Tests + `examples/probe_hexpat_corpus`
# read pattern files from there.
#
# Usage: scripts/fetch_imhex_patterns.sh [DEST]
#   DEST defaults to `.imhex-patterns/` at the repo root.

set -euo pipefail

# Resolve the repo root so the script works from any cwd.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DEST="${1:-$REPO_ROOT/.imhex-patterns}"
REPO_URL="https://github.com/WerWolv/ImHex-Patterns.git"

if [ -d "$DEST/.git" ]; then
    echo "fast-forwarding $DEST"
    git -C "$DEST" fetch --depth 1 origin
    git -C "$DEST" reset --hard origin/HEAD
else
    echo "cloning $REPO_URL into $DEST"
    mkdir -p "$(dirname "$DEST")"
    git clone --depth 1 "$REPO_URL" "$DEST"
fi

echo "done. $DEST is at $(git -C "$DEST" rev-parse --short HEAD)"
