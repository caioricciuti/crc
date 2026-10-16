# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh multi-cursor-box.

# ---- Option-drag selects a box, and the line commands keep every cursor ----
# Option-click adds a caret. Option-drag from column 4 of line 1 to column 9
# of line 4 selects those columns on each line; the `x` line ends before
# the box and gets no cursor. Tab then indents the three lines, typing
# replaces the three selections, Cmd-/ comments the three lines and
# Shift-Tab outdents them, each at every cursor and as one undo step.
printf 'let alpha = 1;\nlet delta = 2;\nx\nlet gamma = 3;\n' > "$T/mcbox.txt"
cat > "$T/mcbox.script" <<SCRIPT
clickat @cell.2.0 0 0.5 opt
dump $T/mcbox0.out
downat @cell.1.4 0 0.5 opt
dragat @cell.3.7 0 0.5 opt
frame $T/mcbox-drag.bmp
dragat @cell.4.9 0 0.5 opt
dump $T/mcbox1.out
upat @cell.4.9 0 0.5 opt
key 48
dump $T/mcbox2.out
text v
frame $T/mcbox-typed.bmp
dump $T/mcbox3.out
key 44 cmd /
dump $T/mcbox4.out
key 48 shift
dump $T/mcbox5.out
key 6 cmd z
dump $T/mcbox6.out
quit
SCRIPT
CRC_SELFTEST="$T/mcbox.script" "$BIN" "$T/mcbox.txt" 2> "$T/mcbox.err" || {
    echo "FAIL: the app died playing the box selection script (exit $?)"
    fail=1
}
expect "$T/mcbox0.out" cursors "2"
# The pointer's row is the primary.
expect "$T/mcbox1.out" cursors "3"
expect "$T/mcbox1.out" selection '"gamma"'
expect "$T/mcbox2.out" cursors "3"
expect "$T/mcbox2.out" selection '"gamma"'
expect_line "$T/mcbox2.out" 1 "    let alpha = 1;"
expect_line "$T/mcbox2.out" 3 "x"
expect_line "$T/mcbox2.out" 4 "    let gamma = 3;"
expect "$T/mcbox3.out" cursors "3"
expect_line "$T/mcbox3.out" 1 "    let v = 1;"
expect_line "$T/mcbox3.out" 2 "    let v = 2;"
expect_line "$T/mcbox3.out" 3 "x"
expect_line "$T/mcbox3.out" 4 "    let v = 3;"
expect "$T/mcbox4.out" cursors "3"
expect_line "$T/mcbox4.out" 1 "    // let v = 1;"
expect_line "$T/mcbox4.out" 3 "x"
expect_line "$T/mcbox4.out" 4 "    // let v = 3;"
expect "$T/mcbox5.out" cursors "3"
expect_line "$T/mcbox5.out" 2 "// let v = 2;"
expect_line "$T/mcbox5.out" 4 "// let v = 3;"
expect "$T/mcbox6.out" cursors "3"
expect_line "$T/mcbox6.out" 2 "    // let v = 2;"
expect_line "$T/mcbox6.out" 4 "    // let v = 3;"
[ -z "${SELFTEST_FRAMES:-}" ] || cp -f "$T/mcbox-drag.bmp" "$T/mcbox-typed.bmp" "$SELFTEST_FRAMES/"
