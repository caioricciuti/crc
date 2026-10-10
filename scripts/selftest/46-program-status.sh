# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh program-status.

# ---- Program status (OSC 7501): the agent's own word on its state ----------
# A stand-in agent probes for the protocol, prints the reply it got, then
# reports blocked on a permission, working and done. The terminal marks
# the tab from the reports, not from output or hooks, and typing into it
# takes a result as seen (the echo of the typing counts as output, so
# the tab reads working for three seconds, then idle). Claude Code reports
# this way since 2.1.295.
mkdir -p "$T/statusproj"
printf 'one\n' > "$T/statusproj/a.txt"
msg=$(printf 'Run cargo test?' | base64)
cat > "$T/status-agent.sh" <<AGENT
#!/bin/sh
stty -icanon -echo min 1 time 10 2>/dev/null
printf '\033]7501;?\033\\\\'
reply=\$(dd bs=10 count=1 2>/dev/null | od -An -c | tr -d ' \n')
stty sane 2>/dev/null
printf 'reply=%s\n' "\$reply"
printf '\033]7501;state=blocked:kind=permission:app=claude-code:msg=$msg\033\\\\'
sleep 2
printf '\033]7501;state=working:app=claude-code\033\\\\'
sleep 2
printf '\033]7501;state=done:app=claude-code\033\\\\'
sleep 30
AGENT
chmod +x "$T/status-agent.sh"
cat > "$T/status.script" <<SCRIPT
wait 500
key 8 cmd,shift C
wait 700
wait 700
dump $T/status-blocked.out
wait 700
wait 700
wait 700
dump $T/status-working.out
wait 700
wait 700
wait 700
dump $T/status-done.out
text y
wait 700
wait 700
wait 700
wait 700
wait 700
dump $T/status-seen.out
quit
SCRIPT
CRC_TERMINAL_SHELL=/bin/sh CRC_CLAUDE_COMMAND="$T/status-agent.sh" \
    CRC_SELFTEST="$T/status.script" "$BIN" "$T/statusproj" 2> "$T/status.err"
expect_terminal "$T/status-blocked.out" 'reply=033]7501;?033'
expect "$T/status-blocked.out" terminals "waiting: Run cargo test?"
expect "$T/status-working.out" terminals "working"
expect "$T/status-done.out" terminals "waiting: finished"
expect "$T/status-seen.out" terminals "idle"
