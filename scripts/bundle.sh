#!/usr/bin/env bash
# Build a Lintel.app bundle and code-sign it.
#
# WHY THIS MATTERS (design v2 §9.2): the Accessibility (TCC) grant is keyed to the app's
# code-signature. An ad-hoc / unsigned build's cdhash changes every rebuild, so the grant
# silently resets. Signing every build with ONE stable identity keeps the grant across rebuilds.
# A persistent *self-signed* cert is enough for local use (Developer ID + notarization is only
# needed to distribute to other Macs).
#
# Usage:
#   scripts/bundle.sh                       # ad-hoc sign (grant will reset on rebuild)
#   LINTEL_SIGN_ID="Lintel Dev" scripts/bundle.sh   # stable identity (grant persists)
#
# Create a reusable self-signed code-signing identity once, via Keychain Access:
#   Keychain Access ▸ Certificate Assistant ▸ Create a Certificate…
#     Name: "Lintel Dev"   Identity Type: Self Signed Root   Certificate Type: Code Signing
#   then:  export LINTEL_SIGN_ID="Lintel Dev"
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release
APP="target/Lintel.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp target/release/lintel "$APP/Contents/MacOS/lintel"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Lintel</string>
  <key>CFBundleDisplayName</key><string>Lintel</string>
  <key>CFBundleIdentifier</key><string>com.ddriver.lintel</string>
  <key>CFBundleVersion</key><string>0.1.0</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleExecutable</key><string>lintel</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSUIElement</key><true/>
  <key>LSMinimumSystemVersion</key><string>26.0</string>
</dict>
</plist>
PLIST

SIGN_ID="${LINTEL_SIGN_ID:--}"   # default: ad-hoc ("-")
codesign --force --sign "$SIGN_ID" "$APP"

echo "Built $APP  (signed with identity: '$SIGN_ID')"
codesign -dvv "$APP" 2>&1 | grep -E 'Identifier|Authority|Signature' || true
echo
echo "Run the bundle:            open $APP        (watch mode; LSUIElement = no Dock icon)"
echo "Or the inner binary:       $APP/Contents/MacOS/lintel read"
echo "First run needs Accessibility: System Settings ▸ Privacy & Security ▸ Accessibility ▸ add Lintel."
