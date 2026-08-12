#!/usr/bin/env bash
# bundle-mac.sh — assemble a macOS .app bundle for Jade (Phase-5b).
#
# Idempotent: rebuilds the release binary, tears down any prior bundle, and
# reassembles Jade.app from scratch under rust/target/bundle/. Ad-hoc codesigned
# (identity "-"), matching the Electron build's `mac.identity: null`.
#
#   Output: rust/target/bundle/Jade.app
#
# Fonts: any TTF/OTF in this repo's Resources source dir (see FONT_SRC below) is
# copied into Jade.app/Contents/Resources/fonts and loaded at startup by
# crates/jade/src/fonts.rs. None are vendored today (no download allowed), so the
# app resolves Menlo — drop JetBrainsMono-*.ttf into that dir and re-run to ship
# JetBrains Mono. See rust/README.md "Packaging".
set -euo pipefail

# ── Paths ─────────────────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$RUST_DIR/.." && pwd)"

APP_NAME="Jade"
VERSION="0.1.0"
BUNDLE_ID="com.jade.ide"          # same id as the old Electron app, so macOS keeps its permissions
MIN_MACOS="13.0"

TARGET_DIR="${CARGO_TARGET_DIR:-$RUST_DIR/target}"
BUNDLE_DIR="$TARGET_DIR/bundle"
APP="$BUNDLE_DIR/$APP_NAME.app"
BIN_SRC="$TARGET_DIR/release/jade"
ICON_SRC="$REPO_ROOT/build-resources/icon.icns"
# Where a human drops font TTFs to be bundled (empty today, checked into git via
# a .gitkeep). Kept next to the script so the bundle is reproducible from repo.
FONT_SRC="$SCRIPT_DIR/resources/fonts"

echo "==> Jade bundle: $APP"

# ── 1. Build the release binary ───────────────────────────────────────────────
echo "==> cargo build --release -p jade"
( cd "$RUST_DIR" && cargo build --release -p jade )
[ -x "$BIN_SRC" ] || { echo "ERROR: release binary missing at $BIN_SRC" >&2; exit 1; }

# ── 2. Assemble the bundle skeleton (idempotent: nuke + recreate) ─────────────
echo "==> assembling bundle"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
mkdir -p "$APP/Contents/Resources/fonts"

cp "$BIN_SRC" "$APP/Contents/MacOS/jade"
chmod +x "$APP/Contents/MacOS/jade"

# Icon (reuse the Electron build's icns).
if [ -f "$ICON_SRC" ]; then
  cp "$ICON_SRC" "$APP/Contents/Resources/icon.icns"
else
  echo "WARN: icon not found at $ICON_SRC — bundling without CFBundleIconFile asset" >&2
fi

# ── 3. Fonts ──────────────────────────────────────────────────────────────────
font_count=0
if [ -d "$FONT_SRC" ]; then
  shopt -s nullglob nocaseglob
  for f in "$FONT_SRC"/*.ttf "$FONT_SRC"/*.otf; do
    cp "$f" "$APP/Contents/Resources/fonts/"
    font_count=$((font_count + 1))
  done
  shopt -u nullglob nocaseglob
fi
if [ "$font_count" -gt 0 ]; then
  echo "==> bundled $font_count font file(s) from $FONT_SRC"
else
  echo "==> no fonts to bundle (Resources/fonts empty) — app resolves Menlo"
fi

# ── 4. Info.plist ─────────────────────────────────────────────────────────────
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key>
	<string>$APP_NAME</string>
	<key>CFBundleDisplayName</key>
	<string>$APP_NAME</string>
	<key>CFBundleIdentifier</key>
	<string>$BUNDLE_ID</string>
	<key>CFBundleExecutable</key>
	<string>jade</string>
	<key>CFBundleIconFile</key>
	<string>icon.icns</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>$VERSION</string>
	<key>CFBundleVersion</key>
	<string>$VERSION</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>LSMinimumSystemVersion</key>
	<string>$MIN_MACOS</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.developer-tools</string>
</dict>
</plist>
PLIST

# ── 5. Ad-hoc codesign ────────────────────────────────────────────────────────
# Identity "-" = ad-hoc (no Developer ID), matching the Electron mac.identity:null.
echo "==> codesign --sign - (ad-hoc)"
codesign --force --deep --sign - --timestamp=none "$APP"
codesign -dv "$APP" 2>&1 | sed 's/^/    /' || true

echo "==> done: $APP"
du -sh "$APP" | sed 's/^/    /'
