# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh empty-states.

# ---- a panel with nothing to show says so, the one way --------------------
# A clean repository and an mcp.json with no servers each draw the shared
# empty state: an icon, a title, a hint and at most one button, centred. The dumps prove the states are
# the empty ones; the frames are for the eye, and go to $SELFTEST_FRAMES
# when that is set.
mkdir -p "$T/emptyproj" "$T/emptyhome/.config/crc"
printf 'one\ntwo\n' > "$T/emptyproj/notes.txt"
git init -q -b main "$T/emptyproj"
git -C "$T/emptyproj" add notes.txt
testgit "$T/emptyproj" commit -q -m notes
printf '{"mcpServers": {}}\n' > "$T/emptyhome/.config/crc/mcp.json"
cat > "$T/empty.script" <<SCRIPT
resize 1440 900
wait 800
key 5 cmd,opt g
wait 900
dump $T/empty-git.out
frame $T/empty-git.bmp
key 5 cmd,opt g
wait 200
key 35 cmd p
wait 200
text >mcp servers
key 36
wait 400
dump $T/empty-mcp.out
frame $T/empty-mcp.bmp
quit
SCRIPT
HOME="$T/emptyhome" CRC_SELFTEST="$T/empty.script" "$BIN" "$T/emptyproj" 2> "$T/empty.err"
[ -z "${SELFTEST_FRAMES:-}" ] || cp -f "$T/empty-git.bmp" "$T/empty-mcp.bmp" "$SELFTEST_FRAMES/"
expect "$T/empty-git.out" git_open true
expect "$T/empty-git.out" git_changes 0
grep -q '^activity: mcp ' "$T/empty-mcp.out" \
    || failed "empty-mcp.out: $(grep '^activity:' "$T/empty-mcp.out")"
expect "$T/empty-mcp.out" mcp ""
