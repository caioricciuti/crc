# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh terminal-clear.

# ---- terminal clear -----------------------------------------------------------
# Cmd-K clears the terminal here, without typing anything into it: the
# history and the screen go, the prompt's line moves to the top, and the
# shell carries on below it. A full-screen program keeps its screen; the
# shell's history under it goes all the same.
mkdir -p "$T/clearproj"
printf 'untouched\n' > "$T/clearproj/notes.txt"
# A full-screen program: the alternate screen for three seconds.
printf '%s\n' "printf '\\033[?1049h\\033[Hfull screen'" 'sleep 3' "printf '\\033[?1049l'" \
    > "$T/clearproj/full.sh"
cat > "$T/clear.script" <<SCRIPT
wait 500
click @toolbar.terminal
wait 700
wait 700
text cd $T/clearproj
key 36
text seq 1 200
key 36
wait 500
wait 500
dump $T/clear-before.out
key 40 cmd k
wait 300
dump $T/clear-cleared.out
text echo after
key 36
wait 500
wait 500
dump $T/clear-after.out
text seq 1 100; sh full.sh
key 36
wait 700
wait 300
key 40 cmd k
wait 300
dump $T/clear-alternate.out
wait 700
wait 700
wait 700
wait 700
dump $T/clear-back.out
quit
SCRIPT
CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/clear.script" "$BIN" "$T/clearproj/notes.txt" 2> "$T/clear.err"
expect_terminal "$T/clear-before.out" '\n199\n200'
if grep -m1 '^terminal: ' "$T/clear-before.out" | grep -qF ' scrollback=0 '; then
    failed "clear-before.out: seq 1 200 left no history: $(grep -m1 '^terminal: ' "$T/clear-before.out")"
fi
expect_terminal "$T/clear-cleared.out" ' scrollback=0 back=0'
if grep -m1 '^terminal: ' "$T/clear-cleared.out" | grep -qF '200'; then
    failed "clear-cleared.out: the output is still there: $(grep -m1 '^terminal: ' "$T/clear-cleared.out")"
fi
expect_terminal "$T/clear-after.out" '\nafter'
if grep -m1 '^terminal: ' "$T/clear-after.out" | grep -qF '199'; then
    failed "clear-after.out: the cleared output came back: $(grep -m1 '^terminal: ' "$T/clear-after.out")"
fi
expect_terminal "$T/clear-alternate.out" 'screen="full screen"'
# What the shell had scrolled into history before the program started
# is gone when it ends: at most the prompt's own line went up since.
back_history=$(grep -m1 '^terminal: ' "$T/clear-back.out" | sed -n 's/.* scrollback=\([0-9]*\) .*/\1/p')
if [ -z "$back_history" ] || [ "$back_history" -gt 2 ]; then
    failed "clear-back.out: the shell's history under the program was kept: scrollback=[$back_history]"
fi
expect_line "$T/clear-back.out" 1 "untouched"
