#!/bin/bash
# Builds "Continuity Settings.app" — universal (Apple silicon + Intel) —
# into <output dir>. CI then copies it into Continuity.app/Contents/Helpers/,
# where continuityd's "Settings…" menu item looks for it.
#
#   ./build-app.sh <output dir> [version] [AppIcon.icns]
#
# Liquid Glass needs the macOS 26 SDK (Xcode 26); with an older Xcode this
# still builds, with the classic look everywhere.
set -euo pipefail

OUT=${1:?usage: build-app.sh <output dir> [version] [AppIcon.icns]}
VERSION=${2:-0.0.0}
ICON=${3:-}
cd "$(dirname "$0")"

swift build -c release --arch arm64 --arch x86_64 --product ContinuitySettings
BIN="$(swift build -c release --arch arm64 --arch x86_64 --show-bin-path)/ContinuitySettings"

APP="$OUT/Continuity Settings.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/ContinuitySettings"
cp Info.plist "$APP/Contents/Info.plist"
/usr/libexec/PlistBuddy \
  -c "Set :CFBundleVersion $VERSION" \
  -c "Set :CFBundleShortVersionString $VERSION" \
  "$APP/Contents/Info.plist"
if [ -n "$ICON" ]; then
  cp "$ICON" "$APP/Contents/Resources/AppIcon.icns"
fi
lipo -info "$APP/Contents/MacOS/ContinuitySettings"
