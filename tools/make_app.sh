#!/bin/bash
# Builds a double-clickable VoiceKeyboard.app bundle.
# No Apple Developer account needed for local use (ad-hoc signed).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT/src-tauri"

echo "==> Building release binary…"
cargo build --release

APP="$ROOT/dist/VoiceKeyboard.app"
CONTENTS="$APP/Contents"
MACOS="$CONTENTS/MacOS"
RES="$CONTENTS/Resources"

echo "==> Assembling $APP"
rm -rf "$APP"
mkdir -p "$MACOS" "$RES"
cp target/release/voicekeyboard "$MACOS/VoiceKeyboard"
cp icons/icon.icns "$RES/icon.icns"
cp icons/icon.png "$RES/icon.png"

# Minimal Info.plist (LSUIElement = menu-bar app, no Dock icon).
cat > "$CONTENTS/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleExecutable</key>
	<string>VoiceKeyboard</string>
	<key>CFBundleIdentifier</key>
	<string>com.voicekeyboard.app</string>
	<key>CFBundleName</key>
	<string>VoiceKeyboard</string>
	<key>CFBundleDisplayName</key>
	<string>VoiceKeyboard</string>
	<key>CFBundleVersion</key>
	<string>0.1.0</string>
	<key>CFBundleShortVersionString</key>
	<string>0.1.0</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleIconFile</key>
	<string>icon.icns</string>
	<key>LSMinimumSystemVersion</key>
	<string>10.15</string>
	<key>LSUIElement</key>
	<true/>
	<key>NSMicrophoneUsageDescription</key>
	<string>VoiceKeyboard records audio while you hold the hotkey to transcribe your speech locally.</string>
	<key>NSAccessibilityUsageDescription</key>
	<string>VoiceKeyboard needs Accessibility access to paste transcribed text into the focused app and to detect password fields.</string>
</dict>
</plist>
PLIST

echo "==> Ad-hoc signing…"
codesign --force --deep --sign - "$APP"

echo
echo "Done. Launch with:"
echo "  open \"$APP\""
echo "or move it to /Applications."
