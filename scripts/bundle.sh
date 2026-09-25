#!/usr/bin/env bash
# Builds crc.app, a real macOS application bundle.
#
# A bare Mach-O binary works, but macOS treats it as a second-class citizen:
# no Dock icon of its own, no Finder double-click, no "Open With", and an
# activation policy that fights you. A bundle is what turns the binary into
# something you can actually live in.
#
# Usage:
#   scripts/bundle.sh              build into target/crc.app, ad-hoc signed
#   scripts/bundle.sh --install    build, then copy into /Applications
#   scripts/bundle.sh --release    build, sign with the Developer ID in the
#                                  keychain, notarize, staple, and write a
#                                  signed, notarized DMG plus SHA256SUMS
#
# --release needs two things only the release machine has, both entered by
# a person and never by a script: a "Developer ID Application" identity in
# the login keychain, and a notarytool keychain profile named crc-notary
# (`xcrun notarytool store-credentials crc-notary`). Neither is read from
# the environment or the repository.
set -euo pipefail
cd "$(dirname "$0")/.."

APP_NAME="crc"
BUNDLE_ID="dev.ricciuti.crc"
VERSION=$(grep -m1 '^version = ' Cargo.toml | sed 's/version = //; s/"//g')
APP="target/${APP_NAME}.app"

echo "==> building release binary"
cargo build --release --locked

echo "==> assembling ${APP}"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "target/release/${APP_NAME}" "$APP/Contents/MacOS/${APP_NAME}"

echo "==> generating icon"
python3 scripts/make-icon.py "$APP/Contents/Resources/${APP_NAME}.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>                  <string>${APP_NAME}</string>
    <key>CFBundleDisplayName</key>           <string>${APP_NAME}</string>
    <key>CFBundleIdentifier</key>            <string>${BUNDLE_ID}</string>
    <key>CFBundleVersion</key>               <string>${VERSION}</string>
    <key>CFBundleShortVersionString</key>    <string>${VERSION}</string>
    <key>CFBundleExecutable</key>            <string>${APP_NAME}</string>
    <key>CFBundleIconFile</key>              <string>${APP_NAME}</string>
    <key>CFBundlePackageType</key>           <string>APPL</string>
    <key>CFBundleInfoDictionaryVersion</key> <string>6.0</string>

    <!-- Metal and the 2x atlas both assume a real backing scale. Without
         this the window server hands us a 1x framebuffer and scales it up,
         which undoes the whole point of rasterizing at device resolution. -->
    <key>NSHighResolutionCapable</key>       <true/>

    <key>LSMinimumSystemVersion</key>        <string>14.0</string>
    <key>NSSupportsAutomaticTermination</key> <false/>
    <key>NSSupportsSuddenTermination</key>   <false/>

    <!-- Claim plain text and source files so "Open With" lists us. Viewer
         rather than Editor: we can open these, but until there is a Save As
         and a proper dirty-close prompt, claiming to own them overstates it. -->
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key>      <string>Text Document</string>
            <key>CFBundleTypeRole</key>      <string>Viewer</string>
            <key>LSHandlerRank</key>         <string>Alternate</string>
            <key>LSItemContentTypes</key>
            <array>
                <string>public.plain-text</string>
                <string>public.source-code</string>
                <string>public.script</string>
                <string>public.data</string>
                <string>public.folder</string>
            </array>
        </dict>
    </array>
</dict>
</plist>
PLIST

# The alpha ships under 30 MB. Grammars are most of the binary, so growth is
# printed per grammar on every build and a bundle over the line is a failure,
# not a surprise at release time. The objects are named by hash; the grammar
# is read from the symbol each one exports.
echo "==> size"
for object in target/release/build/crc-*/out/*-parser.o; do
    [ -f "$object" ] || continue
    name=$(nm -g "$object" 2>/dev/null | sed -n 's/.* T _tree_sitter_\([a-z_]*\)$/\1/p' | head -1)
    [ -n "$name" ] || continue
    printf '    %-14s %6d KB\n' "$name" "$(( $(stat -f %z "$object") / 1024 ))"
done | sort -k2 -n -r | uniq
BUNDLE_KB=$(du -sk "$APP" | cut -f1)
LIMIT_KB=$((30 * 1024))
printf '    bundle         %6d KB (limit %d KB)\n' "$BUNDLE_KB" "$LIMIT_KB"
if [ "$BUNDLE_KB" -gt "$LIMIT_KB" ]; then
    echo "error: ${APP} is over the 30 MB alpha limit" >&2
    exit 1
fi

MODE="${1:-}"
NOTARY_PROFILE="crc-notary"
# On a release runner the profile lives in a throwaway keychain rather
# than the login one; the workflow says where.
NOTARY_KEYCHAIN="${CRC_NOTARY_KEYCHAIN:+--keychain $CRC_NOTARY_KEYCHAIN}"

if [ "$MODE" = "--release" ]; then
    # The identity is looked up, not configured: there is exactly one
    # Developer ID Application certificate on the release machine, and a
    # second one would be a question to answer, not a choice to make here.
    IDENTITY=$(security find-identity -v -p codesigning \
        | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p')
    if [ "$(printf '%s\n' "$IDENTITY" | grep -c .)" -ne 1 ]; then
        echo "error: expected exactly one Developer ID Application identity, found:" >&2
        printf '%s\n' "$IDENTITY" >&2
        exit 1
    fi
    echo "==> signing as $IDENTITY"
    # Inside-out, never --deep: the executable first, then the bundle that
    # contains it. Hardened runtime and a secure timestamp are what
    # notarization requires. The app spawns git, shells and language servers
    # and compiles its Metal shader through the system compiler; none of
    # that needs an entitlement.
    codesign --force --options runtime --timestamp --sign "$IDENTITY" \
        "$APP/Contents/MacOS/${APP_NAME}"
    codesign --force --options runtime --timestamp --sign "$IDENTITY" "$APP"
    codesign --verify --deep --strict --verbose=2 "$APP" 2>&1 | sed 's/^/    /'

    echo "==> notarizing the app"
    ZIP="target/${APP_NAME}-notary.zip"
    rm -f "$ZIP"
    ditto -c -k --keepParent "$APP" "$ZIP"
    # shellcheck disable=SC2086
    xcrun notarytool submit "$ZIP" --keychain-profile "$NOTARY_PROFILE" $NOTARY_KEYCHAIN --wait 2>&1 | sed 's/^/    /'
    rm -f "$ZIP"
    xcrun stapler staple "$APP" 2>&1 | sed 's/^/    /'
    spctl -a -vv -t exec "$APP" 2>&1 | sed 's/^/    /'

    echo "==> disk image"
    DMG="target/${APP_NAME}-${VERSION}.dmg"
    STAGE="target/dmg"
    rm -rf "$STAGE" "$DMG"
    mkdir -p "$STAGE"
    cp -R "$APP" "$STAGE/"
    ln -s /Applications "$STAGE/Applications"
    hdiutil create -quiet -volname "$APP_NAME" -srcfolder "$STAGE" -ov -format ULFO "$DMG"
    rm -rf "$STAGE"
    codesign --force --timestamp --sign "$IDENTITY" "$DMG"
    # shellcheck disable=SC2086
    xcrun notarytool submit "$DMG" --keychain-profile "$NOTARY_PROFILE" $NOTARY_KEYCHAIN --wait 2>&1 | sed 's/^/    /'
    xcrun stapler staple "$DMG" 2>&1 | sed 's/^/    /'
    spctl -a -vv -t open --context context:primary-signature "$DMG" 2>&1 | sed 's/^/    /'
    (cd target && shasum -a 256 "$(basename "$DMG")" > SHA256SUMS)
    echo "    $DMG ($(( $(stat -f %z "$DMG") / 1024 )) KB)"
    echo "    target/SHA256SUMS"
else
    # An unsigned bundle gets quarantined and Gatekeeper-blocked on first
    # launch. Ad-hoc signing is enough for a locally built app and avoids
    # that entirely.
    echo "==> ad-hoc signing"
    codesign --force --deep --sign - "$APP"
    codesign --verify --verbose "$APP" 2>&1 | sed 's/^/    /'
fi

if [ "$MODE" = "--install" ]; then
    echo "==> installing to /Applications"
    rm -rf "/Applications/${APP_NAME}.app"
    cp -R "$APP" /Applications/
    echo "    /Applications/${APP_NAME}.app"
fi

echo
echo "built ${APP}"
echo "  open it:          open ${APP}"
echo "  open a file:      open -a ${APP} path/to/file"
echo "  install:          scripts/bundle.sh --install"
echo "  release:          scripts/bundle.sh --release"
