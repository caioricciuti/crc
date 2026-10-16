# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh find-replace-multi-cursor.

# ---- Replace with several cursors -------------------------------------------
# Three selections of "cat" from Cmd-D, then Find and Replace. Replace went
# through the multi-cursor insert and put the replacement at every cursor;
# it must replace the current match only and leave one cursor, which then
# steps on to the next match.
printf 'cat dog\ncat bird\ncat fish\n' > "$T/find-replace-mc.txt"
cat > "$T/find-replace-mc.script" <<SCRIPT
key 2 cmd d
key 2 cmd d
key 2 cmd d
dump $T/find-replace-mc-before.out
key 3 cmd f
key 48
text x
key 36
dump $T/find-replace-mc.out
quit
SCRIPT
CRC_SELFTEST="$T/find-replace-mc.script" "$BIN" "$T/find-replace-mc.txt" 2> "$T/find-replace-mc.err" || {
    echo "FAIL: the app died playing the find-replace multi-cursor script (exit $?)"
    fail=1
}
expect "$T/find-replace-mc-before.out" cursors "3"
expect_line "$T/find-replace-mc.out" 1 "x dog"
expect_line "$T/find-replace-mc.out" 2 "cat bird"
expect_line "$T/find-replace-mc.out" 3 "cat fish"
expect "$T/find-replace-mc.out" cursors "1"
expect "$T/find-replace-mc.out" selection '"cat"'

# ---- the same with a regex and a capture group ------------------------------
# The bar is set up first (regex on, a query with a group, a replacement),
# then the document gets the keys and three cursors, and the Replace button
# is pressed: the capture must expand for the current match alone.
cat > "$T/find-replace-mc-regex.script" <<SCRIPT
key 3 cmd f
clickin @find 620 13
text c(a)t
key 48
text _\$1_
click 344 400
key 126 cmd
key 2 cmd d
key 2 cmd d
key 2 cmd d
dump $T/find-replace-mc-regex-before.out
clickin @find 542 47
dump $T/find-replace-mc-regex.out
quit
SCRIPT
CRC_SELFTEST="$T/find-replace-mc-regex.script" "$BIN" "$T/find-replace-mc.txt" 2> "$T/find-replace-mc-regex.err" || {
    echo "FAIL: the app died playing the find-replace regex multi-cursor script (exit $?)"
    fail=1
}
expect "$T/find-replace-mc-regex-before.out" cursors "3"
expect_line "$T/find-replace-mc-regex.out" 1 "_a_ dog"
expect_line "$T/find-replace-mc-regex.out" 2 "cat bird"
expect_line "$T/find-replace-mc-regex.out" 3 "cat fish"
expect "$T/find-replace-mc-regex.out" cursors "1"
