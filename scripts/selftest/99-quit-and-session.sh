# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh quit-and-session.

# ---- Don't Save, then Cancel, keeps every tab --------------------------------
# Two documents with unsaved changes and Cmd-Q: Don't Save for the first,
# Cancel for the second. The quit is off and both tabs are still there, the
# first included: nothing closes until every question is answered. A second
# Cmd-Q asks again (the answers run out at Cancel), so the window stays.
mkdir -p "$T/quitproj"
printf 'one\n' > "$T/quitproj/a.txt"
printf 'two\n' > "$T/quitproj/b.txt"
cat > "$T/quit.script" <<SCRIPT
wait 600
key 35 cmd p
text b.txt
wait 300
key 36
wait 300
text changed
key 35 cmd p
text a.txt
wait 300
key 36
wait 300
text changed
wait 100
key 12 cmd q
wait 300
dump $T/quit-kept.out
key 12 cmd q
wait 300
dump $T/quit-again.out
quit
SCRIPT
CRC_DISCARD_ANSWER=dont-save,cancel CRC_SELFTEST="$T/quit.script" "$BIN" "$T/quitproj" 2> "$T/quit.err"
expect "$T/quit-kept.out" tabs "b.txt | a.txt"
expect "$T/quit-kept.out" active 1
expect "$T/quit-kept.out" dirty "true"
expect "$T/quit-again.out" tabs "b.txt | a.txt"

# ---- the session is written as the tabs change, not only at quit ------------
# A file opened, then the process killed outright, as a crash or the power
# going would. The session used to be written at quit alone, so the next
# launch showed the tabs of the last clean quit. It is written a moment
# after the tabs change: the file names what was open before the kill. The
# script keeps the app alive with run-loop waits (the display link does the
# writing) until the shell has seen the file and killed it; its own quit
# writes nothing, so a late kill cannot pass the check for it.
session="$HOME/Library/Application Support/crc/session"
real=$(cd "$T/quitproj" && pwd -P)
rm -f "$session"
cat > "$T/session.script" <<SCRIPT
wait 600
key 35 cmd p
text b.txt
wait 300
key 36
idle 2500
idle 2500
idle 2500
idle 2500
quit
SCRIPT
CRC_SELFTEST_SESSION=1 CRC_SELFTEST="$T/session.script" "$BIN" "$T/quitproj" 2> "$T/session.err" &
session_pid=$!
for _ in $(seq 1 60); do
    grep -qF "file=$real/b.txt" "$session" 2>/dev/null && break
    sleep 0.1
done
kill -9 "$session_pid" 2>/dev/null || true
wait "$session_pid" 2>/dev/null || true
grep -qF "file=$real/b.txt" "$session" 2>/dev/null \
    || failed "session: not written while the app ran: $(cat "$session" 2>/dev/null)"
# The folder is kept as it was given; the files are canonical.
grep -qF "folder=$real" "$session" 2>/dev/null || grep -qF "folder=$T/quitproj" "$session" 2>/dev/null \
    || failed "session: the folder is missing: $(cat "$session" 2>/dev/null)"
# Later scenarios launched on nothing must not find these tabs.
rm -f "$session"
