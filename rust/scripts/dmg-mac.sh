#!/usr/bin/env bash
# dmg-mac.sh — build a distributable UDZO .dmg from Jade.app (Phase-5b).
#
# Requires Jade.app to already exist (run bundle-mac.sh first). Stages the app
# plus an /Applications symlink into a temp dir and hdiutil-creates a compressed
# read-only image.
#
#   Output: rust/target/bundle/Jade-0.1.0-arm64.dmg   (volume "Jade 0.1.0")
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

APP_NAME="Jade"
VERSION="0.1.0"
ARCH="arm64"
VOL_NAME="$APP_NAME $VERSION"

TARGET_DIR="${CARGO_TARGET_DIR:-$RUST_DIR/target}"
BUNDLE_DIR="$TARGET_DIR/bundle"
APP="$BUNDLE_DIR/$APP_NAME.app"
DMG="$BUNDLE_DIR/$APP_NAME-$VERSION-$ARCH.dmg"

[ -d "$APP" ] || { echo "ERROR: $APP not found — run bundle-mac.sh first" >&2; exit 1; }

echo "==> staging dmg contents"
STAGE="$(mktemp -d "${TMPDIR:-/tmp}/jade-dmg.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT
# ditto preserves the codesignature/xattrs when copying the .app.
ditto "$APP" "$STAGE/$APP_NAME.app"
ln -s /Applications "$STAGE/Applications"

echo "==> hdiutil create (UDZO) -> $DMG"
rm -f "$DMG"
hdiutil create \
  -volname "$VOL_NAME" \
  -srcfolder "$STAGE" \
  -fs HFS+ \
  -format UDZO \
  -ov \
  "$DMG"

echo "==> done: $DMG"
du -sh "$DMG" | sed 's/^/    /'
