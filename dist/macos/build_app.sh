#!/usr/bin/env bash
# Assemble target/release/bundle/macos/hxy.app from a cargo-release
# build + the static Info.plist next to this script. Hand-rolled
# instead of cargo-bundle so we don't add a build-time tooling
# dependency just to copy three files into a directory tree.
#
# Usage:
#   dist/macos/build_app.sh            # release build, no signing
#   PROFILE=debug dist/macos/build_app.sh
#   CODESIGN_IDENTITY="Developer ID Application: ..." dist/macos/build_app.sh

set -euo pipefail

PROFILE="${PROFILE:-release}"
case "$PROFILE" in
  release) CARGO_PROFILE_FLAG="--release"; TARGET_DIR_SUBDIR="release" ;;
  debug)   CARGO_PROFILE_FLAG="";          TARGET_DIR_SUBDIR="debug" ;;
  *) echo "unknown PROFILE=$PROFILE (expected release|debug)" >&2; exit 1 ;;
esac

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
BUNDLE_DIR="$TARGET_DIR/bundle/macos"
APP_DIR="$BUNDLE_DIR/hxy.app"
APP_CONTENTS="$APP_DIR/Contents"
APP_MACOS="$APP_CONTENTS/MacOS"
APP_RESOURCES="$APP_CONTENTS/Resources"

echo "building hxy ($PROFILE) ..."
( cd "$REPO_ROOT" && cargo build -p hxy $CARGO_PROFILE_FLAG --bin hxy )

BIN_PATH="$TARGET_DIR/$TARGET_DIR_SUBDIR/hxy"
if [[ ! -f "$BIN_PATH" ]]; then
  echo "missing binary at $BIN_PATH" >&2
  exit 1
fi

echo "assembling $APP_DIR ..."
rm -rf "$APP_DIR"
mkdir -p "$APP_MACOS" "$APP_RESOURCES"
cp "$BIN_PATH" "$APP_MACOS/hxy"
chmod +x "$APP_MACOS/hxy"
cp "$SCRIPT_DIR/Info.plist" "$APP_CONTENTS/Info.plist"
# Optional icon: dist/macos/hxy.icns is copied if present. Without
# it Finder shows the generic app icon, which is fine for dev.
if [[ -f "$SCRIPT_DIR/hxy.icns" ]]; then
  cp "$SCRIPT_DIR/hxy.icns" "$APP_RESOURCES/hxy.icns"
fi

if [[ -n "${CODESIGN_IDENTITY:-}" ]]; then
  echo "codesigning with identity: $CODESIGN_IDENTITY ..."
  codesign --force --options runtime --sign "$CODESIGN_IDENTITY" "$APP_DIR"
fi

# Register the bundle with Launch Services so the right-click
# "Open With" submenu picks it up without requiring a logout. lsregister
# lives in a private framework path; suppress its noisy output but
# don't fail the build if it's missing (rare CI minimal images).
LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
if [[ -x "$LSREGISTER" ]]; then
  "$LSREGISTER" -f "$APP_DIR" >/dev/null 2>&1 || true
fi

echo "done: $APP_DIR"
