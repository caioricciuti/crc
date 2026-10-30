# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh receipt.

# ---- The receipt: what a session's agent ran, as its hooks said ---------
# A stand-in agent reports three shell commands through the Bash hooks
# Claude Code runs: `echo ok` succeeded (PostToolUse), `cargo test`
# failed with exit 101 (PostToolUseFailure), and a dev server went to the
# background. In Agents mode the inspector's Receipt card counts three,
# one failed, and lists the newest first. The frames are for a look at
# the card in both themes.
case "$BIN" in /*) rc_bin=$BIN ;; *) rc_bin=$PWD/$BIN ;; esac
P="$T/receiptproj"
mkdir -p "$P"
printf 'x\n' > "$P/a.txt"
testgit "$P" init -q -b main
testgit "$P" add a.txt
testgit "$P" commit -qm one
cat > "$T/receipt-agent.sh" <<AGENT
#!/bin/sh
printf '{"tool_name":"Bash","tool_input":{"command":"echo ok"},"tool_response":{"stdout":"ok"}}' | "$rc_bin" --hook ran
printf '{"tool_name":"Bash","tool_input":{"command":"cargo test --offline"},"error":"Exit code 101\\\\nerror: 1 test failed","is_interrupt":false}' | "$rc_bin" --hook ran-failed
printf '{"tool_name":"Bash","tool_input":{"command":"npm run dev","run_in_background":true},"tool_response":{"backgroundTaskId":"b1"}}' | "$rc_bin" --hook ran
printf 'done\n'
sleep 30
AGENT
chmod +x "$T/receipt-agent.sh"

# rc_run <name> <theme>: one crc run with the stand-in, in a home of its own.
rc_run() {
    cat > "$T/receipt-$1.script" <<SCRIPT
wait 500
key 8 cmd,shift C
wait 700
wait 700
key 0 cmd,shift A
wait 700
wait 500
dump $T/receipt-$1.out
frame $T/receipt-$1.png
quit
SCRIPT
    mkdir -p "$T/home/receipt-$1/.config/crc"
    printf 'theme = "%s"\n' "$2" > "$T/home/receipt-$1/.config/crc/config.toml"
    HOME="$T/home/receipt-$1" CRC_TERMINAL_SHELL=/bin/sh CRC_CLAUDE_COMMAND="$T/receipt-agent.sh" \
        CRC_SELFTEST="$T/receipt-$1.script" "$BIN" "$P" 2> "$T/receipt-$1.err"
}
rc_run dark dark
expect "$T/receipt-dark.out" receipt "ran=3 failed=1 rows=3 last=bg:npm run dev"
rc_run light light
expect "$T/receipt-light.out" receipt "ran=3 failed=1 rows=3 last=bg:npm run dev"
