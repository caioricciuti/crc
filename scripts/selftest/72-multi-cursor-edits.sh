# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh multi-cursor-edits.

# ---- deletes and Return at every cursor -------------------------------------
# Three carets from Cmd-D on lines indented differently. Option-Backspace and
# Option-Delete used to act at the primary only and drop the others, and
# Return gave every line the primary's indentation.
printf 'total a\n    total b\n        total c\n' > "$T/multicursor.txt"
cat > "$T/multicursor.script" <<SCRIPT
key 2 cmd d
key 2 cmd d
key 2 cmd d
text word
key 51 opt
dump $T/multicursor1.out
key 117 opt
key 36
frame $T/multicursor.bmp
text x
dump $T/multicursor2.out
quit
SCRIPT
CRC_SELFTEST="$T/multicursor.script" "$BIN" "$T/multicursor.txt" 2> "$T/multicursor.err" || {
    echo "FAIL: the app died playing the multi-cursor script (exit $?)"
    fail=1
}
expect "$T/multicursor1.out" cursors "3"
expect_line "$T/multicursor1.out" 1 " a"
expect_line "$T/multicursor1.out" 2 "     b"
expect_line "$T/multicursor1.out" 3 "         c"
expect "$T/multicursor2.out" cursors "3"
expect_line "$T/multicursor2.out" 1 ""
expect_line "$T/multicursor2.out" 2 "x"
expect_line "$T/multicursor2.out" 3 "    "
expect_line "$T/multicursor2.out" 4 "    x"
expect_line "$T/multicursor2.out" 5 "        "
expect_line "$T/multicursor2.out" 6 "        x"
[ -z "${SELFTEST_FRAMES:-}" ] || cp -f "$T/multicursor.bmp" "$SELFTEST_FRAMES/"
