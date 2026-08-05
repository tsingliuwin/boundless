#!/usr/bin/env bash
# Assemble Boundless.app from a release binary for local testing.
#
# Mirrors the macOS packaging step in .github/workflows/release.yml so what
# you test locally matches what CI ships.
#
# Usage: scripts/package-macos.sh [version]
#   version defaults to "0.0.0-local".

set -euo pipefail

cd "$(dirname "$0")/.."

VERSION="${1:-0.0.0-local}"
TARGET="aarch64-apple-darwin"
APP="Boundless.app"

echo "Building release binary for $TARGET…"
cargo build --release --target "$TARGET"

echo "Assembling $APP (v$VERSION)…"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "target/$TARGET/release/boundless" "$APP/Contents/MacOS/boundless"
cp assets/icon.icns "$APP/Contents/Resources/icon.icns"
cp assets/Info.plist "$APP/Contents/Info.plist"
chmod +x "$APP/Contents/MacOS/boundless"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" "$APP/Contents/Info.plist"

echo "Done. Launch with: open $APP"
