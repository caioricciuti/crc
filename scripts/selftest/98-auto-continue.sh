# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh auto-continue.

# ---- Auto-continue: a turn ended by a temporary API error is tried again ---
# A stand-in agent ends its turn with the StopFailure hook, the error in
# AC_ERROR, then echoes each line typed into it as got:<line>. A server
# error is retried: the tab says retrying, then crc types "continue" and
# Return into it. Typed into before the try comes due, it is the person's
# and nothing is sent; a billing error is never retried. The first wait is
# cut to a second and a half for the test.
case "$BIN" in /*) ac_bin=$BIN ;; *) ac_bin=$PWD/$BIN ;; esac
mkdir -p "$T/acproj"
printf 'x\n' > "$T/acproj/a.txt"
cat > "$T/ac-agent.sh" <<AGENT
#!/bin/sh
printf '{"hook_event_name":"StopFailure","error":"%s","error_details":"API Error: %s"}' \
    "\$AC_ERROR" "\$AC_ERROR" | "$ac_bin" --hook stop-failure
printf 'stub ready\n'
while IFS= read -r line; do
    printf 'got:%s\n' "\$line"
done
AGENT
chmod +x "$T/ac-agent.sh"

# ac_run <name> <error> <script lines>: one crc run with the stand-in,
# in a home of its own, so no run gets back an earlier one's session.
ac_run() {
    printf '%s\n' "$3" > "$T/ac-$1.script"
    mkdir -p "$T/home/ac-$1"
    HOME="$T/home/ac-$1" AC_ERROR=$2 CRC_AUTO_CONTINUE_DELAY_MS=1500 \
        CRC_TERMINAL_SHELL=/bin/sh CRC_CLAUDE_COMMAND="$T/ac-agent.sh" CRC_SELFTEST="$T/ac-$1.script" \
        "$BIN" "$T/acproj" 2> "$T/ac-$1.err"
}
# ac_sent <file>: whether the stand-in got "continue".
ac_sent() {
    grep -m1 '^terminal: ' "$1" | grep -qF 'got:continue'
}

ac_run retry server_error "wait 500
key 8 cmd,shift C
wait 700
dump $T/ac-retrying.out
wait 700
wait 700
wait 700
dump $T/ac-sent.out
quit"
expect "$T/ac-retrying.out" terminals "retrying: API Error: server_error"
expect_terminal "$T/ac-sent.out" 'got:continue'
ac_sent "$T/ac-sent.out" && grep -m1 '^terminal: ' "$T/ac-sent.out" | grep -qF 'got:continue\ngot:' \
    && failed "ac-sent.out: continue sent more than once"

ac_run typed server_error "wait 500
key 8 cmd,shift C
wait 700
text x
wait 700
wait 700
wait 700
dump $T/ac-typed.out
quit"
ac_sent "$T/ac-typed.out" && failed "ac-typed.out: continue sent after the person typed"
expect_terminal "$T/ac-typed.out" 'stub ready'

ac_run billing billing_error "wait 500
key 8 cmd,shift C
wait 700
wait 700
wait 700
wait 700
dump $T/ac-billing.out
quit"
expect "$T/ac-billing.out" terminals "waiting: API Error: billing_error"
ac_sent "$T/ac-billing.out" && failed "ac-billing.out: a billing error was retried"
expect_terminal "$T/ac-billing.out" 'stub ready'
