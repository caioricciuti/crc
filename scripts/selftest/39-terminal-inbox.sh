# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh inbox.

# ---- what each terminal waits on ------------------------------------------------
# A program that asks for attention with an OSC 9 notification (what Codex
# and Claude Code send when told to) marks its tab; when the person is not
# typing into that tab, the status line says which session asked and what.
# Typing into the tab answers it. The bell marks a tab the same way.
mkdir -p "$T/inboxproj"
printf 'untouched\n' > "$T/inboxproj/notes.txt"
cat > "$T/inbox.script" <<SCRIPT
wait 500
click @toolbar.terminal
wait 700
wait 700
key 8 cmd,shift C
wait 300
click @text
wait 1200
wait 600
wait 300
dump $T/inbox-asked.out
key 8 cmd,shift C
wait 300
text y
wait 300
dump $T/inbox-answered.out
quit
SCRIPT
CRC_TERMINAL_SHELL=/bin/sh \
    CRC_CLAUDE_COMMAND="sleep 1; printf '\\033]9;Claude needs your permission to use Bash\\007'; sleep 30" \
    CRC_SELFTEST="$T/inbox.script" "$BIN" "$T/inboxproj/notes.txt" 2> "$T/inbox.err"
expect "$T/inbox-asked.out" terminals "idle|waiting: Claude needs your permission to use Bash"
grep -q '^message: .*: waiting: Claude needs your permission to use Bash$' "$T/inbox-asked.out" \
    || failed "inbox-asked.out: the status line did not say it: $(grep '^message:' "$T/inbox-asked.out")"
# The document underneath was never typed into.
expect_line "$T/inbox-asked.out" 1 "untouched"
grep -q '^terminals: .*waiting' "$T/inbox-answered.out" \
    && failed "inbox-answered.out: typing into the tab did not answer it: $(grep '^terminals:' "$T/inbox-answered.out")"

# The bell marks the tab that rang it. The scripted keys cannot carry a
# backslash, so the shell stand-in rings it itself.
printf '#!/bin/sh\nprintf "ready\\a"\nsleep 30\n' > "$T/bell.sh"
chmod +x "$T/bell.sh"
cat > "$T/bell.script" <<SCRIPT
wait 500
click @toolbar.terminal
wait 700
wait 700
dump $T/bell.out
quit
SCRIPT
CRC_TERMINAL_SHELL="$T/bell.sh" CRC_SELFTEST="$T/bell.script" "$BIN" "$T/inboxproj/notes.txt" 2> "$T/bell.err"
expect "$T/bell.out" terminals "waiting: rang the bell"
