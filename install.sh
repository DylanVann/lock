#!/bin/sh
# Builds lock and installs it: the `lock` and `lock-gui` commands onto your PATH, and on
# macOS the Lock app into /Applications. Rerun it to update; a running Lock app is quit
# and reopened.
#
#   ./install.sh                    # commands into ~/.local/bin, app into /Applications
#   BIN_DIR=/usr/local/bin APP_DIR=~/Applications ./install.sh
set -eu
cd "$(dirname "$0")"
BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"
APP_DIR="${APP_DIR:-/Applications}"

cargo build --release --locked -p lock -p lock-gui

mkdir -p "$BIN_DIR"
for bin in lock lock-gui; do
    # Copy then rename: replacing the file (rather than writing into it) avoids writing
    # through an old symlink and macOS killing an overwritten signed binary.
    tmp="$BIN_DIR/.$bin.tmp.$$"
    cp "target/release/$bin" "$tmp"
    chmod 755 "$tmp"
    mv -f "$tmp" "$BIN_DIR/$bin"
    echo "installed $BIN_DIR/$bin"
done

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "note: $BIN_DIR is not on your PATH" ;;
esac

[ "$(uname)" = Darwin ] || exit 0

macos/build.sh
mkdir -p "$APP_DIR"
if [ ! -w "$APP_DIR" ]; then
    echo "can't write to $APP_DIR; set APP_DIR, e.g. APP_DIR=~/Applications ./install.sh" >&2
    exit 1
fi

# Quit a running copy (wherever it was started from) so it isn't replaced underneath itself.
was_running=false
if pgrep -f "Lock.app/Contents/MacOS/Lock\$" >/dev/null; then
    was_running=true
    osascript -e 'tell application id "com.dylanvann.lock" to quit' >/dev/null 2>&1 || true
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        pgrep -f "Lock.app/Contents/MacOS/Lock\$" >/dev/null || break
        sleep 0.5
    done
fi

# Copy beside the old one, then swap, so there's never a half-copied app in place.
tmp="$APP_DIR/.Lock.app.tmp.$$"
rm -rf "$tmp"
ditto macos/build/Lock.app "$tmp"
rm -rf "$APP_DIR/Lock.app" "$APP_DIR/CPU Lock.app" "$HOME/Applications/CPU Lock.app"
mv "$tmp" "$APP_DIR/Lock.app"
echo "installed $APP_DIR/Lock.app"

if $was_running; then
    open "$APP_DIR/Lock.app"
fi
