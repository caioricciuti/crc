# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh preview-pane.

# ---- the preview pane ---------------------------------------------------------
# Markdown Preview, built in crc-extensions, installed from a folder. Cmd-E
# splits the pane: the text keeps the left half, the page the right. What
# the page may load is checked from inside it: the image beside the
# document loads, one outside the folder does not, a remote image the page
# is made to load (Markdown Preview itself writes remote images as text) is
# blocked by crc's rules, and a link goes nowhere. The remote image is one
# that exists (it loads at 64 pixels without the rules), so a 0 is the
# rules and not a missing file. An edit re-renders without moving the page's scroll;
# the palette hides the page while it is up; Cmd-E again closes it and
# deletes the file it was loaded from. The divider between text and page
# drags; Cmd-Shift-E gives the page the whole tab, where typing reaches
# nothing, and Escape brings the split back.
X="$PWD/tests/fixtures/extensions/markdown-preview"
mkdir -p "$T/pvhome" "$T/pvproj/img" "$T/pvfolder" "$T/pvreg"
cp "$X"/* "$T/pvfolder/"
python3 - "$T" <<'PY'
import pathlib, struct, sys, zlib
t = pathlib.Path(sys.argv[1])
def png(w, h):
    raw = b"".join(b"\0" + b"\x80\x40\x20" * w for _ in range(h))
    chunk = lambda k, d: struct.pack(">I", len(d)) + k + d + struct.pack(">I", zlib.crc32(k + d))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))
(t / "pvproj/img/dot.png").write_bytes(png(64, 64))
(t / "outside.png").write_bytes(png(32, 32))
(t / "pvproj/doc.md").write_text(
    "# Title\n\n![here](img/dot.png) ![remote](https://example.com/x.png) ![out](../outside.png)\n\n"
    "[a link](img/dot.png)\n\n" + "".join(f"Paragraph {i} with *some* text.\n\n" for i in range(200)))
PY
cat > "$T/pv.script" <<SCRIPT
wait 600
key 7 cmd,shift x
idle 900
click @extensions.folder
wait 200
click @extensions.confirm
idle 600
key 53
key 14 cmd e
idle 2500
webjs probe
idle 300
dump $T/pv-open.out
down @preview.divider
dragby -150 0
upby -150 0
idle 300
dump $T/pv-dragged.out
key 14 cmd,shift E
idle 600
dump $T/pv-only.out
text Z
idle 300
dump $T/pv-only-typed.out
key 53
idle 300
dump $T/pv-split.out
webjs (function(){var i=new Image();i.id='remote';i.src='https://www.apple.com/favicon.ico';document.body.appendChild(i);return 'added'})()
idle 1500
webjs document.getElementById('remote').naturalWidth + ' ' + document.querySelector('.remote-image').textContent
idle 300
dump $T/pv-remote.out
webjs (document.querySelector('a').click(), 'clicked')
idle 600
webjs location.href.indexOf('/Library/Caches/crc/preview-') > 0 ? 'stayed' : location.href
idle 300
dump $T/pv-link.out
webjs (scrollTo(0,300),scrollY)
idle 300
key 126 cmd
text X
idle 1500
webjs probe
idle 300
dump $T/pv-edited.out
key 35 cmd p
idle 300
dump $T/pv-palette.out
key 53
idle 300
dump $T/pv-back.out
key 14 cmd e
idle 300
dump $T/pv-closed.out
quit
SCRIPT
HOME="$T/pvhome" CRC_EXT_REGISTRY="file://$T/pvreg/" CRC_EXT_FOLDER="$T/pvfolder" CRC_SELFTEST="$T/pv.script" "$BIN" "$T/pvproj/doc.md" 2> "$T/pv.err"
expect "$T/pv-open.out" preview "open ext=crc.markdown-preview view=shown probe=64/0 scroll=0 h1=Title text=6113"
expect "$T/pv-remote.out" preview "open ext=crc.markdown-preview view=shown probe=0 remote"
# 1100 wide, sidebar 240: the text's 816 points become 407, a gap, 408.
grep -q '^layout: window 1100x760 sidebar Some(240.0) text 284,116 407x616 ' "$T/pv-open.out" \
    || failed "preview: the text did not give the page its half: $(grep '^layout:' "$T/pv-open.out")"
# The divider dragged 150 points left: the text narrower, the page wider.
grep -Eq '^layout: .* text 284,116 2[4-7][0-9]x616 preview Some\(5[4-7][0-9]\.0\) ' "$T/pv-dragged.out" \
    || failed "preview: the divider did not move: $(grep '^layout:' "$T/pv-dragged.out")"
# Preview only: the page has the column, and typing does not reach the text.
grep -q '^layout: .* text 284,116 0x616 preview Some(816.0) ' "$T/pv-only.out" \
    || failed "preview: the page did not take the tab: $(grep '^layout:' "$T/pv-only.out")"
expect_line "$T/pv-only-typed.out" 1 "# Title"
# Escape brings the text back at the width it was dragged to.
[ "$(grep '^layout:' "$T/pv-split.out")" = "$(grep '^layout:' "$T/pv-dragged.out")" ] \
    || failed "preview: Escape did not bring the split back: $(grep '^layout:' "$T/pv-split.out")"
expect "$T/pv-link.out" preview "open ext=crc.markdown-preview view=shown probe=stayed"
expect "$T/pv-edited.out" preview "open ext=crc.markdown-preview view=shown probe=64/0 scroll=300 h1=- text=6116"
expect "$T/pv-palette.out" preview "open ext=crc.markdown-preview view=veiled probe=64/0 scroll=300 h1=- text=6116"
expect "$T/pv-back.out" preview "open ext=crc.markdown-preview view=shown probe=64/0 scroll=300 h1=- text=6116"
expect "$T/pv-closed.out" preview "closed"
[ -z "$(ls "$T/pvhome/Library/Caches/crc" 2>/dev/null | grep '^preview-')" ] \
    || failed "preview: page files left behind: $(ls "$T/pvhome/Library/Caches/crc")"
