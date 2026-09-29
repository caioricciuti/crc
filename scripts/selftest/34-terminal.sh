# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh terminal.

# ---- terminal -----------------------------------------------------------------
# The toolbar's Terminal button opens a shell in a panel under the editor;
# typing runs a command there and leaves the document alone. The panel's top
# edge drags taller. Cmd-Shift-C starts a Claude Code tab, and every session
# is told this window's IDE port; CRC_CLAUDE_COMMAND stands in for claude
# and prints what it was given. Cmd-W closes a tab, and a program that exits
# closes its own, taking the panel with the last one.
mkdir -p "$T/termproj"
printf 'untouched\n' > "$T/termproj/notes.txt"
cat > "$T/term.script" <<SCRIPT
wait 500
click @toolbar.terminal
wait 700
wait 700
text echo abc def
key 36
wait 500
wait 500
dump $T/term-shell.out
down @terminal.divider
dragby 0 -100
upby 0 -100
wait 200
dump $T/term-resized.out
key 8 cmd,shift C
wait 700
wait 700
wait 700
dump $T/term-claude.out
key 13 cmd w
wait 300
dump $T/term-closed.out
text exit
key 36
wait 500
wait 500
dump $T/term-exit.out
key 50 ctrl \`
wait 700
wait 700
dump $T/term-again.out
quit
SCRIPT
CRC_TERMINAL_SHELL=/bin/sh CRC_CLAUDE_COMMAND='echo port=$CLAUDE_CODE_SSE_PORT ide=$ENABLE_IDE_INTEGRATION; sleep 30' \
    CRC_SELFTEST="$T/term.script" "$BIN" "$T/termproj/notes.txt" 2> "$T/term.err"
expect_terminal "$T/term-shell.out" "open=true focus=true height=320 tabs=sh "
expect_terminal "$T/term-shell.out" '\nabc def'
expect_line "$T/term-shell.out" 1 "untouched"
expect "$T/term-shell.out" dirty false
expect_terminal "$T/term-resized.out" "height=420 "
expect_terminal "$T/term-claude.out" "tabs=sh|✻ Claude "
if ! grep -m1 '^terminal: ' "$T/term-claude.out" | grep -qE 'port=[0-9]+ ide=true'; then
    echo "FAIL term-claude.out: the Claude session was not given the IDE port: $(grep -m1 '^terminal: ' "$T/term-claude.out")"
    fail=1
fi
expect_terminal "$T/term-closed.out" "tabs=sh "
expect_terminal "$T/term-exit.out" "open=false focus=false height=420 tabs= "
expect_terminal "$T/term-again.out" "open=true focus=true height=420 tabs=sh "

# ---- terminal selection and file references ---------------------------------
# A triple click selects a line of output; Cmd-click on a path:line printed
# in the terminal opens that file at that line. The output is on screen row
# 1 after `clear`, since row 0 holds the command.
printf 'one\ntwo\nthree\n' > "$T/termproj/second.txt"
cat > "$T/termsel.script" <<SCRIPT
wait 500
click @toolbar.terminal
wait 700
wait 700
text clear
key 36
wait 500
text echo second.txt:2
key 36
wait 500
wait 500
clickin @terminal 30 24 3
wait 200
dump $T/termsel-line.out
clickin @terminal 30 24 1 cmd
wait 500
wait 500
dump $T/termsel-open.out
quit
SCRIPT
CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/termsel.script" "$BIN" "$T/termproj/notes.txt" 2> "$T/termsel.err"
expect_terminal "$T/termsel-line.out" 'selection=Some("second.txt:2")'
expect "$T/termsel-open.out" tabs "notes.txt | second.txt"
expect "$T/termsel-open.out" cursor "2:1"
expect_terminal "$T/termsel-open.out" "focus=false"
