#!/usr/bin/env bash
# Launch the hxy binary from inside a dev bundle dir (arg 1), so
# current_exe() sits beside the bundle's plugins/ and template-plugins/
# dirs and a debug build discovers them. Remaining args pass through.
set -euo pipefail
bundle="$1"
shift
exec "$bundle/hxy" "$@"
