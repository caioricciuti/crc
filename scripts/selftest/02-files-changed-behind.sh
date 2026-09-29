# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh files-changed-behind.

# ---- files changed behind the editor --------------------------------------
# A clean tab follows the disk; a tab with unsaved changes keeps them and is
# told. This is the path Claude Code and git checkout take.
printf 'first\n' > "$T/behind.txt"
cat > "$T/behind.script" <<SCRIPT
write $T/behind.txt second
wait 100
dump $T/behind-clean.out
text local
write $T/behind.txt third
wait 100
dump $T/behind-dirty.out
key 6 cmd z
key 6 cmd z
dump $T/behind-undone.out
quit
SCRIPT
CRC_SELFTEST="$T/behind.script" "$BIN" "$T/behind.txt" 2> "$T/behind.err"
expect_line "$T/behind-clean.out" 1 "second"
expect "$T/behind-clean.out" dirty "false"
expect_line "$T/behind-dirty.out" 1 "localsecond"
expect "$T/behind-dirty.out" dirty "true"
if [ "$(sed -n 1p "$T/behind.txt")" != "third" ]; then
    echo "FAIL behind: the external write was replaced by [$(sed -n 1p "$T/behind.txt")]"
    fail=1
fi
# Two undos: the typed word, then the reload itself.
expect_line "$T/behind-undone.out" 1 "first"
expect "$T/behind-undone.out" dirty "true"
