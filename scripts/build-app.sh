#!/usr/bin/env bash
# Build a macOS .app bundle that wraps the `mouse-share-ui` release binary.
#
# Why an .app?
#   macOS only grants Accessibility (Input Monitoring + event-tap) permission
#   to an application the user can toggle in System Settings. A raw binary
#   run from a terminal is identified by its parent tty, which makes the
#   permission grant both confusing and fragile across rebuilds. Bundling
#   the UI binary inside `mouse share.app` with a stable bundle identifier
#   gives macOS a single, identifiable target for the permission grant that
#   survives rebuilds.
#
# Usage:
#   ./scripts/build-app.sh            # release build → dist/mouse share.app
#   open "dist/mouse share.app"
#
# Requirements: cargo, codesign (ships with macOS).

set -euo pipefail

if [[ "$(uname)" != "Darwin" ]]; then
    echo "build-app.sh: this script only supports macOS" >&2
    exit 1
fi

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_ROOT"

APP_NAME="mouse share"
BUNDLE_ID="com.xunfei.mouse-share"
BIN_NAME="mouse-share-ui"
CLI_BIN_NAME="mouse-share"
VERSION="$(grep -E '^version' Cargo.toml | head -1 | sed -E 's/.*"([^"]+)".*/\1/')"

# Honour a user-wide `[build] target-dir = ...` in ~/.cargo/config.toml — we
# ask cargo for the effective target directory instead of hardcoding `target`.
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$(cargo metadata --format-version 1 --no-deps 2>/dev/null | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')}"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target}"
RELEASE_DIR="$CARGO_TARGET_DIR/release"

# Bundle output stays alongside the project so users don't have to hunt
# through the user-wide target dir for it.
OUT_DIR="dist"
APP_DIR="$OUT_DIR/$APP_NAME.app"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
RESOURCES_DIR="$CONTENTS_DIR/Resources"

echo "==> Building release binary ($BIN_NAME)..."
cargo build --release --features ui --bin "$BIN_NAME"

# The CLI binary isn't strictly required inside the bundle, but shipping it
# alongside means `mouse share.app/Contents/MacOS/mouse-share` is available
# for users who want headless operation too.
cargo build --release --bin "$CLI_BIN_NAME"

echo "==> Assembling $APP_DIR"
rm -rf "$APP_DIR"
mkdir -p "$MACOS_DIR" "$RESOURCES_DIR"

cp "$RELEASE_DIR/$BIN_NAME" "$MACOS_DIR/$BIN_NAME"
cp "$RELEASE_DIR/$CLI_BIN_NAME" "$MACOS_DIR/$CLI_BIN_NAME"
chmod +x "$MACOS_DIR/$BIN_NAME" "$MACOS_DIR/$CLI_BIN_NAME"

cat > "$CONTENTS_DIR/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>mouse share</string>
    <key>CFBundleExecutable</key>
    <string>${BIN_NAME}</string>
    <key>CFBundleIdentifier</key>
    <string>${BUNDLE_ID}</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>mouse share</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <!--
        LSUIElement=true hides mouse share from the Dock and the
        cmd-tab switcher. It's still a foreground-capable app (so
        CGDisplayHideCursor works) — it just doesn't clutter Dock
        for something the user normally drives from the small window.
    -->
    <key>LSUIElement</key>
    <true/>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSPrincipalClass</key>
    <string>NSApplication</string>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
    <!--
        Purpose strings shown by macOS in the permission prompts.
        Accessibility and Input Monitoring don't have dedicated
        NSUsageDescription keys (they're granted via System Settings),
        but NSAppleEventsUsageDescription covers the case where we
        programmatically open System Settings from the "permission
        required" screen.
    -->
    <key>NSAppleEventsUsageDescription</key>
    <string>mouse share opens System Settings so you can grant Accessibility permission for input capture.</string>
</dict>
</plist>
PLIST

# Ad-hoc sign so Gatekeeper stops flagging the bundle on first launch and so
# the bundle identity is stable across rebuilds (important for Accessibility
# permission to survive). Distribution signing would use a real Developer ID
# cert here; for local builds an ad-hoc signature is sufficient.
echo "==> Ad-hoc signing..."
codesign --force --deep --sign - "$APP_DIR"

echo
echo "Built: $APP_DIR"
echo "Run:   open \"$APP_DIR\""
echo
echo "First launch: macOS will prompt for Accessibility. Grant it in"
echo "System Settings → Privacy & Security → Accessibility, then relaunch."
