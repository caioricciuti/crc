# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh terminal-find.

# ---- find in the terminal ---------------------------------------------------
# Cmd-F with the terminal's keys opens the find bar over the terminal, not
# the document's: typing finds in its history and screen, the newest match
# first. Cmd-G and Cmd-Shift-G, Return and Shift-Return go round the
# matches, scrolling back to an old one and down again to the output. A
# click in the terminal gives it the keys with the bar left open; Escape in
# the bar closes it, and typing reaches the shell again. The document
# beside it is not touched, and Cmd-F in it still opens its own bar.
mkdir -p "$T/tfindproj"
printf 'untouched\n' > "$T/tfindproj/notes.txt"
{
    echo 'echo alpha needle one'
    echo 'seq 1 150'
    # A line far longer than the terminal is wide: it wraps.
    echo "printf '%0300d needle two\\n' 0"
    echo 'echo omega needle three'
} > "$T/tfindproj/lines.sh"
cat > "$T/tfind.script" <<SCRIPT
wait 500
click @toolbar.terminal
wait 700
wait 700
text cd $T/tfindproj
key 36
text clear; sh lines.sh
key 36
wait 500
wait 500
key 3 cmd f
text needle
wait 200
dump $T/tfind-open.out
key 5 cmd g
wait 200
dump $T/tfind-wrapped.out
key 5 cmd,shift G
wait 200
dump $T/tfind-back.out
key 36 shift
wait 200
dump $T/tfind-previous.out
key 36
wait 200
dump $T/tfind-next.out
clickin @terminal 30 200
wait 200
dump $T/tfind-clicked.out
key 3 cmd f
wait 200
dump $T/tfind-again.out
key 53
wait 200
text echo typed
key 36
wait 500
wait 500
dump $T/tfind-closed.out
key 3 cmd f
wait 200
dump $T/tfind-remembered.out
key 53
click @text
key 3 cmd f
text untouched
wait 200
dump $T/tfind-document.out
quit
SCRIPT
CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/tfind.script" "$BIN" "$T/tfindproj/notes.txt" 2> "$T/tfind.err"
expect_terminal "$T/tfind-open.out" ' back=0 find=needle 3 of 3 target=terminal keys=true'
# Round from the newest to the oldest, far back in the history.
expect_terminal "$T/tfind-wrapped.out" 'find=needle 1 of 3 target=terminal'
if grep -m1 '^terminal: ' "$T/tfind-wrapped.out" | grep -qF ' back=0 '; then
    failed "tfind-wrapped.out: the oldest match was not scrolled into view: $(grep -m1 '^terminal: ' "$T/tfind-wrapped.out" | sed 's/.* scrollback=/scrollback=/')"
fi
expect_terminal "$T/tfind-back.out" ' back=0 find=needle 3 of 3 target=terminal'
expect_terminal "$T/tfind-previous.out" 'find=needle 2 of 3 target=terminal'
expect_terminal "$T/tfind-next.out" 'find=needle 3 of 3 target=terminal'
expect_terminal "$T/tfind-clicked.out" 'find=needle 3 of 3 target=terminal keys=false'
expect_terminal "$T/tfind-again.out" 'find=needle 3 of 3 target=terminal keys=true'
expect_terminal "$T/tfind-closed.out" ' find=none'
expect_terminal "$T/tfind-closed.out" '\ntyped'
# Closed and opened again with nothing selected: the last search, on
# the newest match (the "typed" line holds none).
expect_terminal "$T/tfind-remembered.out" 'find=needle 3 of 3 target=terminal keys=true'
expect_line "$T/tfind-closed.out" 1 "untouched"
expect "$T/tfind-closed.out" dirty false
expect_terminal "$T/tfind-document.out" 'find=untouched target=document keys=true'
expect "$T/tfind-document.out" selection '"untouched"'

# ---- find in an Agents mode terminal ------------------------------------------
# The same bar over a session in Agents mode, where it is the only bar;
# with two side by side, over the one with the keys, searching its own.
mkdir -p "$T/tfindagents"
printf 'one\n' > "$T/tfindagents/a.txt"
testgit "$T/tfindagents" init -q -b main
testgit "$T/tfindagents" add a.txt
testgit "$T/tfindagents" commit -qm one
cp "$T/tfindproj/lines.sh" "$T/tfindagents/lines.sh"
cat > "$T/tfindagents.script" <<SCRIPT
wait 500
key 0 cmd,shift A
wait 700
wait 700
text clear; sh lines.sh
key 36
wait 500
wait 500
key 3 cmd f
text needle
wait 200
dump $T/tfindagents-open.out
key 53
key 42 cmd \\
wait 700
wait 700
text echo needle here
key 36
wait 500
key 3 cmd f
text needle
wait 200
dump $T/tfindagents-split.out
quit
SCRIPT
mkdir -p "$T/home/tfindagents"
HOME="$T/home/tfindagents" CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/tfindagents.script" \
    "$BIN" "$T/tfindagents" 2> "$T/tfindagents.err"
expect_terminal "$T/tfindagents-open.out" 'find=needle 3 of 3 target=terminal keys=true'
# The command and what it printed: the new pane's own two.
expect_terminal "$T/tfindagents-split.out" 'find=needle 2 of 2 target=terminal keys=true'
