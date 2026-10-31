# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh receipt.

# ---- The receipt: what a session's agent ran, as its hooks said ---------
# A stand-in agent is sent a prompt, then reports four shell commands
# through the Bash hooks Claude Code runs: `echo ok` succeeded
# (PostToolUse), `cargo test` failed with exit 101 (PostToolUseFailure),
# `cargo build` succeeded, and a dev server went to the background. Its
# Stop hook then says the tests pass, clippy is clean and it builds. In
# Agents mode the inspector's Receipt card holds each claim up to the
# turn's commands (the tests failed, no lint ran, the build passed),
# counts four commands, one failed, and lists the newest first. The
# frames are for a look at the card in both themes.
case "$BIN" in /*) rc_bin=$BIN ;; *) rc_bin=$PWD/$BIN ;; esac
P="$T/receiptproj"
mkdir -p "$P"
printf 'x\n' > "$P/a.txt"
testgit "$P" init -q -b main
testgit "$P" add a.txt
testgit "$P" commit -qm one
cat > "$T/receipt-agent.sh" <<AGENT
#!/bin/sh
printf '{"prompt":"fix the parser"}' | "$rc_bin" --hook prompt
printf '{"tool_name":"Bash","tool_input":{"command":"echo ok"},"tool_response":{"stdout":"ok"}}' | "$rc_bin" --hook ran
printf '{"tool_name":"Bash","tool_input":{"command":"cargo test --offline"},"error":"Exit code 101\\\\nerror: 1 test failed","is_interrupt":false}' | "$rc_bin" --hook ran-failed
printf '{"tool_name":"Bash","tool_input":{"command":"cargo build"},"tool_response":{"stdout":""}}' | "$rc_bin" --hook ran
printf '{"tool_name":"Bash","tool_input":{"command":"npm run dev","run_in_background":true},"tool_response":{"backgroundTaskId":"b1"}}' | "$rc_bin" --hook ran
printf '{"hook_event_name":"Stop","last_assistant_message":"Fixed the parser.\\\\n\\\\nAll tests pass and clippy is clean. It builds."}' | "$rc_bin" --hook stop
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
expect "$T/receipt-dark.out" receipt "claims=tests:exit 101,lint:not run,build:checked ran=4 failed=1 rows=4 last=bg:npm run dev"
rc_run light light
expect "$T/receipt-light.out" receipt "claims=tests:exit 101,lint:not run,build:checked ran=4 failed=1 rows=4 last=bg:npm run dev"
