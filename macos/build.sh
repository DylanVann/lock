#!/bin/sh
# Builds Lock.app into macos/build/. ../install.sh installs it into /Applications.
set -eu
cd "$(dirname "$0")"

BUILD=build
APP="$BUILD/Lock.app"
FLAGS="-fobjc-arc -fmodules -O2 -Wall -Wextra -Wno-unused-parameter -mmacosx-version-min=14.0"

mkdir -p "$BUILD"
rm -rf "$APP" "$BUILD/CPU Lock.app" # the app's former name
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

clang $FLAGS -framework Cocoa Sources/*.m -o "$APP/Contents/MacOS/Lock"
cp Info.plist "$APP/Contents/Info.plist"

clang $FLAGS -framework Cocoa Tools/make-icon.m -o "$BUILD/make-icon"
rm -rf "$BUILD/AppIcon.iconset"
"$BUILD/make-icon" "$BUILD/AppIcon.iconset"
iconutil -c icns "$BUILD/AppIcon.iconset" -o "$APP/Contents/Resources/AppIcon.icns"

# Bundle the `lock` CLI so Stop and settings changes work without it being on PATH. It goes
# in Contents/Helpers: in Contents/MacOS it would overwrite the app's own `Lock` executable
# on a case-insensitive file system.
(cd .. && cargo build --release --quiet -p lock)
mkdir -p "$APP/Contents/Helpers"
cp ../target/release/lock "$APP/Contents/Helpers/lock"

codesign --force --deep --sign - "$APP" >/dev/null 2>&1
echo "Built $APP"
