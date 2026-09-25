#!/usr/bin/env bash
# Packs an app into a disk image whose window says what to do: a drawn
# background with an arrow, crc on the left, Applications on the right,
# big icons, no toolbar. No tool beyond macOS: hdiutil builds the image,
# Finder lays the window out through AppleScript (which is how it records
# the layout, in the image's .DS_Store), and the image is then compressed
# read-only.
#
# Usage: scripts/make-dmg.sh <app> <dmg>
#
# The window layout needs Finder to be scriptable from whatever runs this.
# If it is not (an Automation permission refused, a machine with no
# session), the image is still made, with a plain window, and this says so:
# a release is never held back for want of a background.
set -euo pipefail
cd "$(dirname "$0")/.."

APP="$1"
DMG="$2"
VOLUME="crc"
WORK=$(mktemp -d)
ATTACHED=0
trap 'if [ "$ATTACHED" = 1 ]; then hdiutil detach -quiet -force "/Volumes/$VOLUME" 2>/dev/null || true; fi; rm -rf "$WORK"' EXIT

if [ -e "/Volumes/$VOLUME" ]; then
    echo "error: a volume named $VOLUME is mounted; eject it first (Finder addresses the window by name)" >&2
    exit 1
fi

echo "    background"
mkdir -p "$WORK/stage/.background"
swift scripts/make-dmg-background.swift "$WORK" 2>&1 | sed 's/^/    /'
tiffutil -cathidpicheck "$WORK/background.png" "$WORK/background@2x.png" \
    -out "$WORK/stage/.background/background.tiff" 2>/dev/null
cp -R "$APP" "$WORK/stage/"
ln -s /Applications "$WORK/stage/Applications"

echo "    image"
SIZE_MB=$(( $(du -sm "$WORK/stage" | cut -f1) + 20 ))
hdiutil create -quiet -volname "$VOLUME" -srcfolder "$WORK/stage" -fs HFS+ \
    -format UDRW -size "${SIZE_MB}m" "$WORK/rw.dmg"
hdiutil attach -quiet -readwrite -noverify -noautoopen \
    -mountpoint "/Volumes/$VOLUME" "$WORK/rw.dmg"
ATTACHED=1

echo "    window layout"
APP_NAME=$(basename "$APP")
if osascript <<APPLESCRIPT
tell application "Finder"
    tell disk "$VOLUME"
        open
        set current view of container window to icon view
        set toolbar visible of container window to false
        set statusbar visible of container window to false
        -- 660 x 420 of content; the title bar is on top of that.
        set the bounds of container window to {200, 120, 860, 568}
        set options to the icon view options of container window
        set arrangement of options to not arranged
        set icon size of options to 128
        set text size of options to 13
        set background picture of options to file ".background:background.tiff"
        set position of item "$APP_NAME" of container window to {180, 190}
        set position of item "Applications" of container window to {480, 190}
        update without registering applications
        delay 1
        close
    end tell
end tell
APPLESCRIPT
then
    # Finder writes .DS_Store when it gets round to it.
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        [ -f "/Volumes/$VOLUME/.DS_Store" ] && break
        sleep 1
    done
    [ -f "/Volumes/$VOLUME/.DS_Store" ] || echo "    warning: Finder did not save the layout; the window will be plain"
else
    echo "    warning: Finder could not be scripted; the window will be plain"
    if [ -n "${GITHUB_ACTIONS:-}" ]; then
        echo "::warning::DMG window layout skipped: Finder could not be scripted"
    fi
fi
# macOS's own bookkeeping for the mounted volume, not part of the image.
rm -rf "/Volumes/$VOLUME/.fseventsd"
chmod -Rf go-w "/Volumes/$VOLUME" 2>/dev/null || true
sync
for _ in 1 2 3 4 5; do
    if hdiutil detach -quiet "/Volumes/$VOLUME"; then
        ATTACHED=0
        break
    fi
    sleep 2
done

echo "    compress"
rm -f "$DMG"
hdiutil convert -quiet "$WORK/rw.dmg" -format ULFO -o "$DMG"
