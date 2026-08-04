#!/bin/bash
# Ship pipeline: build -> sign (Developer ID + hardened runtime) -> DMG -> notarize -> staple.
#
# Credentials (pick one way):
#   A) env vars:  VK_APPLE_ID + VK_NOTARY_PASSWORD (app-specific password)
#   B) keychain:  xcrun notarytool store-credentials "VoiceKeyboard" \
#                   --apple-id "$VK_APPLE_ID" --team-id "$VK_TEAM_ID" --password "$VK_NOTARY_PASSWORD"
#
# Overrides: VK_IDENTITY, VK_TEAM_ID, VK_APPLE_ID, VK_NOTARY_PASSWORD, VK_NOTARY_PROFILE
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT/src-tauri"

IDENTITY="${VK_IDENTITY:-Developer ID Application: XR.DEV.BR LTDA (2JNC7VSQ8N)}"
TEAM_ID="${VK_TEAM_ID:-2JNC7VSQ8N}"
APPLE_ID="${VK_APPLE_ID:-}"
NOTARY_PASSWORD="${VK_NOTARY_PASSWORD:-}"
PROFILE="${VK_NOTARY_PROFILE:-VoiceKeyboard}"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"

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
	<string>${VERSION}</string>
	<key>CFBundleShortVersionString</key>
	<string>${VERSION}</string>
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

echo "==> Signing with '$IDENTITY' (hardened runtime)"
codesign --force --deep --options runtime \
  --entitlements "$ROOT/tools/entitlements.plist" \
  --sign "$IDENTITY" "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"

echo "==> Creating DMG…"
DMG="$ROOT/dist/VoiceKeyboard.dmg"
STAGE="$ROOT/dist/dmg-stage"
rm -rf "$STAGE"
mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
rm -f "$DMG"
hdiutil create -volname "VoiceKeyboard" -srcfolder "$STAGE" -ov -format UDZO "$DMG"
rm -rf "$STAGE"

NOTARY_CMD=()
if [[ -n "$APPLE_ID" && -n "$NOTARY_PASSWORD" ]]; then
  NOTARY_CMD=(--apple-id "$APPLE_ID" --team-id "$TEAM_ID" --password "$NOTARY_PASSWORD")
elif xcrun notarytool history --keychain-profile "$PROFILE" >/dev/null 2>&1; then
  NOTARY_CMD=(--keychain-profile "$PROFILE")
fi

if [[ ${#NOTARY_CMD[@]} -gt 0 ]]; then
  echo "==> Notarizing…"
  xcrun notarytool submit "$DMG" "${NOTARY_CMD[@]}" --wait
  echo "==> Stapling…"
  xcrun stapler staple "$DMG"
  echo "==> Gatekeeper check…"
  spctl --assess --type open --context context:primary-signature -v "$DMG" || echo "spctl: assess the DMG manually (Gatekeeper may need a moment)"
else
  echo "!! Notarization skipped — no credentials."
  echo "   Provide VK_APPLE_ID + VK_NOTARY_PASSWORD (app-specific password),"
  echo "   or store a keychain profile first:"
  echo "     xcrun notarytool store-credentials '$PROFILE' --apple-id YOUR_APPLE_ID --team-id $TEAM_ID --password APP_SPECIFIC_PASSWORD"
fi

echo
echo "Done: $DMG"
