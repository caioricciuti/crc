# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh scrollbar.

# ---- scrollbar ------------------------------------------------------------
# With the sidebar hidden the text pane is 1100x616 at y=116, so the track
# runs x=1091..1096, y=120..728. 401 lines in 32 rows put a 48 pt thumb at
# the top. Grabbing it 10 pt down and dragging 300 pt lands near line 198;
# the exact line depends on the row count, so a window is accepted.
seq 1 400 > "$T/scroll.txt"
cat > "$T/scroll.script" <<SCRIPT
key 11 cmd b
click 1093 400
dump $T/scroll-track.out
down 1093 130
drag 1093 430
up 1093 430
dump $T/scroll-drag.out
quit
SCRIPT
CRC_SELFTEST="$T/scroll.script" "$BIN" "$T/scroll.txt" 2> "$T/scroll.err"
# A press on the track brings the thumb there rather than placing the caret.
expect "$T/scroll-track.out" cursor "1:1"
track=$(grep -m1 '^scroll: ' "$T/scroll-track.out" | sed 's/scroll: //')
if [ "${track:-0}" -lt 150 ] || [ "${track:-0}" -gt 190 ]; then
    echo "FAIL scroll-track.out: scroll is [$track], expected about 170"
    fail=1
fi
drag=$(grep -m1 '^scroll: ' "$T/scroll-drag.out" | sed 's/scroll: //')
if [ "${drag:-0}" -lt 185 ] || [ "${drag:-0}" -gt 210 ]; then
    echo "FAIL scroll-drag.out: scroll is [$drag], expected about 198"
    fail=1
fi
expect "$T/scroll-drag.out" cursor "1:1"

# A trackpad moves the text by points: 30 pt is a line and a half at 19 pt
# rows. A click then lands on the line drawn under it, which is one further
# down than whole-line arithmetic would say.
cat > "$T/smooth.script" <<SCRIPT
key 11 cmd b
trackpad 644 400 -30
click 644 205
dump $T/smooth.out
quit
SCRIPT
CRC_SELFTEST="$T/smooth.script" "$BIN" "$T/scroll.txt" 2> "$T/smooth.err"
expect "$T/smooth.out" scroll "1"
expect "$T/smooth.out" scroll_fraction "0.58"
expect "$T/smooth.out" cursor "7:2"
