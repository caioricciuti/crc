# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh multi-cursor-motions.

# ---- motions keep every cursor ----------------------------------------------
# Three carets from Cmd-D. Any arrow key used to go back to one caret, and
# Down through a short line lost the column each caret came from.
printf 'total one\nx\ntotal two\nx\ntotal three\n' > "$T/mcmotion.txt"
cat > "$T/mcmotion.script" <<SCRIPT
key 2 cmd d
key 2 cmd d
key 2 cmd d
key 124
text X
key 124 cmd
text Y
key 125
dump $T/mcmotion1.out
key 125
frame $T/mcmotion.bmp
text Z
dump $T/mcmotion2.out
quit
SCRIPT
CRC_SELFTEST="$T/mcmotion.script" "$BIN" "$T/mcmotion.txt" 2> "$T/mcmotion.err" || {
    echo "FAIL: the app died playing the multi-cursor motion script (exit $?)"
    fail=1
}
# On the short lines, each caret at the end of its `x` or the empty last line.
expect "$T/mcmotion1.out" cursors "3"
expect_line "$T/mcmotion1.out" 1 "totalX oneY"
expect_line "$T/mcmotion1.out" 3 "totalX twoY"
expect_line "$T/mcmotion1.out" 5 "totalX threeY"
# Down again: the first two back at column 11, the last already at the end.
expect "$T/mcmotion2.out" cursors "3"
expect_line "$T/mcmotion2.out" 2 "x"
expect_line "$T/mcmotion2.out" 3 "totalX twoYZ"
expect_line "$T/mcmotion2.out" 5 "totalX threZeY"
expect_line "$T/mcmotion2.out" 6 "Z"
[ -z "${SELFTEST_FRAMES:-}" ] || cp -f "$T/mcmotion.bmp" "$SELFTEST_FRAMES/"

# ---- copy and paste a line per cursor ---------------------------------------
# Shift-Cmd-Right selects the rest of each line; Cmd-C copies the three, a
# line each, and Cmd-V at three carets puts back one line at each.
printf 'item a\nitem b\nitem c\n' > "$T/mcpaste.txt"
cat > "$T/mcpaste.script" <<SCRIPT
key 2 cmd d
key 2 cmd d
key 2 cmd d
key 124
key 124 cmd,shift
key 8 cmd c
key 123 cmd
key 9 cmd v
dump $T/mcpaste.out
quit
SCRIPT
CRC_SELFTEST="$T/mcpaste.script" "$BIN" "$T/mcpaste.txt" 2> "$T/mcpaste.err" || {
    echo "FAIL: the app died playing the multi-cursor paste script (exit $?)"
    fail=1
}
expect "$T/mcpaste.out" cursors "3"
expect_line "$T/mcpaste.out" 1 " aitem a"
expect_line "$T/mcpaste.out" 2 " bitem b"
expect_line "$T/mcpaste.out" 3 " citem c"
