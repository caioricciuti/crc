# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh one-line.

# ---- terminal copy as one line ------------------------------------------------
# Output broken over two rows with hard newlines and indentation, the way
# Claude Code fits a long command to the width. Select All selects them
# (an empty prompt adds nothing), Cmd-Option-C copies them as one line,
# and pasting that into cat writes a single line. The self-test copies to
# a private pasteboard, not the general one.
mkdir -p "$T/onelineproj"
printf 'untouched\n' > "$T/onelineproj/notes.txt"
printf '  run git commit\n  -F /tmp/msg.txt\n' > "$T/onelineproj/rows.txt"
cat > "$T/oneline.script" <<SCRIPT
wait 500
click @toolbar.terminal
wait 700
wait 700
text cd $T/onelineproj
key 36
text PS1=
key 36
text clear; cat rows.txt
key 36
wait 500
wait 500
key 0 cmd a
wait 200
dump $T/oneline-selected.out
key 8 cmd,opt c
text cat > one.txt
key 36
wait 300
key 9 cmd v
key 36
key 2 ctrl d
wait 500
wait 500
dump $T/oneline-pasted.out
quit
SCRIPT
CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/oneline.script" "$BIN" "$T/onelineproj/notes.txt" 2> "$T/oneline.err"
# Plain Copy would keep both rows.
expect_terminal "$T/oneline-selected.out" 'selection=Some("  run git commit\n  -F /tmp/msg.txt'
if [ "$(cat "$T/onelineproj/one.txt" 2>/dev/null)" != "run git commit -F /tmp/msg.txt" ] \
    || [ "$(wc -l < "$T/onelineproj/one.txt" 2>/dev/null | tr -d ' ')" != 1 ]; then
    failed "one.txt: pasted [$(cat "$T/onelineproj/one.txt" 2>/dev/null)], expected one line [run git commit -F /tmp/msg.txt]"
fi
expect_line "$T/oneline-pasted.out" 1 "untouched"
