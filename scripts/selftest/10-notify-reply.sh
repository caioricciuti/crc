# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh notify.

# ---- A notification when an agent waits, and a reply from it ---------------
# A test instance posts no notification (the framework needs the app
# bundle); the dump counts what it would post and take back, and
# `notify N [text]` answers the Nth session's notification through the
# path the system's callback takes. A shell, then a stand-in agent that
# ends its turn with the Stop hook at the start and after each line typed
# into it, which it echoes as got:<line>. Waiting while it shows, in
# front, it posts nothing; a reply answers it, and the notification is
# taken back. Behind another app, its next wait posts one. A reply to the
# shell runs there, a click shows it, and closing the waiting agent takes
# its notification back.
case "$BIN" in /*) nr_bin=$BIN ;; *) nr_bin=$PWD/$BIN ;; esac
mkdir -p "$T/nrproj" "$T/home/nr-home"
printf 'x\n' > "$T/nrproj/a.txt"
cat > "$T/nr-agent.sh" <<AGENT
#!/bin/sh
printf '{}' | "$nr_bin" --hook stop
printf 'stub ready\n'
while IFS= read -r line; do
    printf 'got:%s\n' "\$line"
    printf '{}' | "$nr_bin" --hook stop
done
AGENT
chmod +x "$T/nr-agent.sh"
cat > "$T/nr.script" <<SCRIPT
wait 500
click @toolbar.terminal
wait 700
wait 700
key 8 cmd,shift C
wait 700
wait 700
wait 700
dump $T/nr-shown.out
notify 2 hello
wait 700
wait 700
dump $T/nr-hello.out
front off
notify 2 again
wait 700
wait 700
dump $T/nr-behind.out
notify 1 echo replied-\$((40 + 2))
wait 700
dump $T/nr-reply.out
notify 1
wait 500
dump $T/nr-click.out
notify 2
wait 300
key 13 cmd w
wait 500
dump $T/nr-closed.out
quit
SCRIPT
HOME="$T/home/nr-home" CRC_TERMINAL_SHELL=/bin/sh CRC_CLAUDE_COMMAND="$T/nr-agent.sh" \
    CRC_SELFTEST="$T/nr.script" "$BIN" "$T/nrproj/a.txt" 2> "$T/nr.err"
# Waiting in front of you: told by showing, nothing posted.
expect_terminal "$T/nr-shown.out" 'stub ready'
expect "$T/nr-shown.out" notified "posted=0 withdrawn=0"
# The reply typed in with its Return: answered, taken back, and its next
# wait, still in front, posts nothing.
expect_terminal "$T/nr-hello.out" 'got:hello'
expect "$T/nr-hello.out" notified "posted=0 withdrawn=1"
# Behind another app, the next wait posts one.
expect_terminal "$T/nr-behind.out" 'got:again'
expect "$T/nr-behind.out" notified "posted=1 withdrawn=2"
if grep -m1 '^terminal: ' "$T/nr-behind.out" | grep -qF 'got:again\ngot:'; then
    failed "nr-behind.out: a reply was typed twice"
fi
# A reply to the shell runs there, behind the agent still showing.
expect_terminal "$T/nr-reply.out" 'got:again'
# A click shows that session: the command the reply ran.
expect_terminal "$T/nr-click.out" '\nreplied-42'
# The waiting agent closed: its notification goes with it.
expect "$T/nr-closed.out" notified "posted=1 withdrawn=3"
if grep -q 'no session' "$T/nr.err"; then
    failed "notify: $(cat "$T/nr.err")"
fi
