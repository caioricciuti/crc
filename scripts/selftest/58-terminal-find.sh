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

# ---- find while a program prints, and filter mode -----------------------------
# The bar stays open over a shell printing two hundred thousand lines: the
# search runs on a worker as the output arrives, and once it stops the
# count is right. Then the lines with a match: Option-Return lists them
# over the screen, on the current match's line (the first after where the
# last query's was); Down and Up choose another and Return jumps to its
# match, scrolled into view, with the list gone. Escape closes the list
# before the bar. The Filter chip opens it too, and a click on a row
# jumps.
mkdir -p "$T/tfindbig"
printf 'untouched\n' > "$T/tfindbig/notes.txt"
cp "$T/tfindproj/lines.sh" "$T/tfindbig/lines.sh"
cat > "$T/tfindbig.script" <<SCRIPT
wait 500
click @toolbar.terminal
wait 700
wait 700
text cd $T/tfindbig
key 36
key 3 cmd f
text 199999
wait 200
clickin @terminal 30 200
text clear; seq 1 200000; sh lines.sh
key 36
wait 1500
wait 1500
wait 1500
dump $T/tfindbig-count.out
key 3 cmd f
text needle
wait 300
key 36 opt
wait 200
dump $T/tfindbig-filter.out
key 125
key 125
key 126
key 36
wait 200
dump $T/tfindbig-picked.out
key 36 opt
wait 200
key 53
wait 200
dump $T/tfindbig-escaped.out
click @find.filter
wait 200
dump $T/tfindbig-chip.out
click @find.filter.row.2
wait 200
dump $T/tfindbig-third.out
key 36 opt
wait 200
click @find.filter.row.1
wait 200
dump $T/tfindbig-clicked.out
quit
SCRIPT
CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/tfindbig.script" "$BIN" "$T/tfindbig/notes.txt" 2> "$T/tfindbig.err"
# The newest line of the output is the only one with 199999; the command
# typed is not in the history any more.
expect_terminal "$T/tfindbig-count.out" 'find=199999 1 of 1 target=terminal keys=false'
if ! grep -qF 'terminal_find: busy=false searching=false ' "$T/tfindbig-count.out"; then
    failed "tfindbig-count.out: a search is still on its way: $(grep -m1 '^terminal_find: ' "$T/tfindbig-count.out")"
fi
# One row per line, the wrapped one once, each match in brackets; the
# current match's line chosen: the first after 199999, which comes before
# the script's output.
filter_rows() {
    grep -m1 '^terminal_find: ' "$1" | sed 's/.* filter=//'
}
if ! filter_rows "$T/tfindbig-filter.out" \
    | grep -qE '^\*[0-9]+ alpha \[needle\] one\|[0-9]+ .0+ \[needle\] two\|[0-9]+ omega \[needle\] three$'; then
    failed "tfindbig-filter.out: the list is [$(filter_rows "$T/tfindbig-filter.out")]"
fi
expect_terminal "$T/tfindbig-filter.out" 'find=needle 1 of 3 target=terminal keys=true'
# Down to the last row, up to the wrapped line, and Return: its match.
expect_terminal "$T/tfindbig-picked.out" 'find=needle 2 of 3 target=terminal keys=true'
expect "$T/tfindbig-picked.out" terminal_find 'busy=false searching=false shown=true filter=off'
expect_terminal "$T/tfindbig-escaped.out" 'find=needle 2 of 3 target=terminal keys=true'
expect "$T/tfindbig-escaped.out" terminal_find 'busy=false searching=false shown=true filter=off'
if ! filter_rows "$T/tfindbig-chip.out" | grep -qE '^[0-9]+ alpha \[needle\] one\|\*[0-9]+ '; then
    failed "tfindbig-chip.out: the list is [$(filter_rows "$T/tfindbig-chip.out")]"
fi
expect_terminal "$T/tfindbig-third.out" 'find=needle 3 of 3 target=terminal keys=true'
expect "$T/tfindbig-third.out" terminal_find 'busy=false searching=false shown=true filter=off'
expect_terminal "$T/tfindbig-clicked.out" 'find=needle 2 of 3 target=terminal keys=true'
expect "$T/tfindbig-clicked.out" terminal_find 'busy=false searching=false shown=true filter=off'
