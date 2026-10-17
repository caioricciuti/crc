# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh restart.

# ---- restart after an install over the running app ---------------------------
# A test instance runs from a bare binary; the bundle it stands in for is
# CRC_UPDATE_APP (as the update scenario uses it), whose executable it
# watches the way a real launch watches its own. Written over while the
# app runs, as an install over it does, the next look (a wait stands in
# for coming to the front) says so: the status line carries the message
# with its Restart button, Home's Start list gets Restart crc, and the dump
# says replaced. Restart quits the way Cmd-Q does; a test instance reopens
# nothing, so the script ends there and the steps after it never run.
mkdir -p "$T/restart/crc.app/Contents/MacOS" "$T/restartproj"
printf 'one\n' > "$T/restart/crc.app/Contents/MacOS/crc"
printf 'fn main() {}\n' > "$T/restartproj/notes.rs"
cat > "$T/restart.script" <<SCRIPT
wait 300
dump $T/restart-before.out
write $T/restart/crc.app/Contents/MacOS/crc two, and longer
wait 300
dump $T/restart.out
click @status.action
wait 500
dump $T/restart-after.out
quit
SCRIPT
CRC_UPDATE_APP="$T/restart/crc.app" CRC_SELFTEST="$T/restart.script" "$BIN" 2> "$T/restart.err"
expect "$T/restart-before.out" replaced "false"
expect "$T/restart-before.out" message ""
expect "$T/restart.out" replaced "true"
expect "$T/restart.out" message "crc was updated on disk"
[ ! -e "$T/restart-after.out" ] || failed "restart: the status line's Restart button did not quit the app"

# Home's row does the same.
cat > "$T/restart-home.script" <<SCRIPT
write $T/restart/crc.app/Contents/MacOS/crc three
wait 300
dump $T/restart-home.out
click @home.restart
wait 500
dump $T/restart-home-after.out
quit
SCRIPT
CRC_UPDATE_APP="$T/restart/crc.app" CRC_SELFTEST="$T/restart-home.script" "$BIN" 2> "$T/restart-home.err"
expect "$T/restart-home.out" replaced "true"
[ ! -e "$T/restart-home-after.out" ] || failed "restart: Home's Restart crc row did not quit the app"

# Help > Restart crc, from the palette, needs no install over the app:
# it quits and reopens whatever is on disk.
cat > "$T/restart-menu.script" <<SCRIPT
key 35 cmd p
text >restart crc
key 36
wait 500
dump $T/restart-menu-after.out
quit
SCRIPT
CRC_UPDATE_APP="$T/restart/crc.app" CRC_SELFTEST="$T/restart-menu.script" "$BIN" 2> "$T/restart-menu.err"
[ ! -e "$T/restart-menu-after.out" ] || failed "restart: Help > Restart crc did not quit the app"

# With unsaved changes the quit asks, as Cmd-Q does, and Cancel (the
# default answer of a test instance) keeps the app running: the restart is
# off, and the message stays, since it is still true.
cat > "$T/restart-cancel.script" <<SCRIPT
text // kept
write $T/restart/crc.app/Contents/MacOS/crc four
wait 300
click @status.action
wait 300
dump $T/restart-cancel.out
quit
SCRIPT
CRC_UPDATE_APP="$T/restart/crc.app" CRC_SELFTEST="$T/restart-cancel.script" "$BIN" "$T/restartproj/notes.rs" 2> "$T/restart-cancel.err"
expect "$T/restart-cancel.out" replaced "true"
expect "$T/restart-cancel.out" dirty "true"
expect "$T/restart-cancel.out" message "crc was updated on disk"
[ "$(cat "$T/restartproj/notes.rs")" = "fn main() {}" ] || failed "restart: a cancelled quit changed the file on disk"
