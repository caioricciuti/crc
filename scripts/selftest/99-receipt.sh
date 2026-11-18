# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh receipt.

# ---- The receipt: what a session's agent ran, as its hooks said ---------
# A stand-in agent is sent a prompt, then reports five shell commands
# through the Bash hooks Claude Code runs: `echo ok` succeeded
# (PostToolUse), `cargo test` failed with exit 101 (PostToolUseFailure)
# after 12 tests passed and 1 failed, `cargo build` succeeded, `npm
# install left-pad` succeeded, and a dev server went to the background.
# Its Stop hook then says the tests pass, clippy is clean and it builds.
# In Agents mode the inspector's Receipt card holds each claim up to the
# turn's commands (the tests failed, 12 passed and 1 failed, no lint ran,
# the build passed), counts five commands, one failed, shows the test
# run's counts, lists the newest first, and lists the install. The frames
# are for a look at the card in both themes. Before its commands the agent
# writes notes.md through the Write hook and commits it, unsigned, which
# the card's Copy Handoff lists with the rest.
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
printf '{"tool_name":"Write","tool_input":{"file_path":"%s/notes.md"}}' "$P" | "$rc_bin" --hook pre
printf 'notes %s\n' "\$\$" >> "$P/notes.md"
git -C "$P" -c user.name=Agent -c user.email=agent@example.com -c commit.gpgsign=false add notes.md
git -C "$P" -c user.name=Agent -c user.email=agent@example.com -c commit.gpgsign=false commit -qm 'agent notes'
printf '{"tool_name":"Bash","tool_input":{"command":"echo ok"},"tool_response":{"stdout":"ok"}}' | "$rc_bin" --hook ran
printf '{"tool_name":"Bash","tool_input":{"command":"cargo test --offline"},"error":"Exit code 101\\\\ntest result: FAILED. 12 passed; 1 failed; 0 ignored\\\\nerror: 1 test failed","is_interrupt":false}' | "$rc_bin" --hook ran-failed
printf '{"tool_name":"Bash","tool_input":{"command":"cargo build"},"tool_response":{"stdout":""}}' | "$rc_bin" --hook ran
printf '{"tool_name":"Bash","tool_input":{"command":"npm install left-pad"},"tool_response":{"stdout":"added 1 package"}}' | "$rc_bin" --hook ran
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
expect "$T/receipt-dark.out" receipt "claims=tests:exit 101=12/1,lint:not run,build:checked ran=5 failed=1 rows=5 counts=12/1 installs=0:npm install left-pad last=bg:npm run dev"
rc_run light light
expect "$T/receipt-light.out" receipt "claims=tests:exit 101=12/1,lint:not run,build:checked ran=5 failed=1 rows=5 counts=12/1 installs=0:npm install left-pad last=bg:npm run dev"

# Closing the terminal keeps its ledger: the workspace's card still counts
# the five commands and the install once the session is gone. Its Copy
# Handoff then puts the session on the clipboard as text: the claims, the
# agent's one commit (not the fixture's, made before the session), the
# install, the test counts, the file written and the failed command.
cat > "$T/receipt-closed.script" <<SCRIPT
wait 500
key 8 cmd,shift C
wait 700
wait 700
wait 700
wait 500
key 13 cmd w
wait 500
key 0 cmd,shift A
wait 700
wait 500
click @inspector.handoff
wait 300
dump $T/receipt-closed.out
quit
SCRIPT
mkdir -p "$T/home/receipt-closed/.config/crc"
HOME="$T/home/receipt-closed" CRC_TERMINAL_SHELL=/bin/sh CRC_CLAUDE_COMMAND="$T/receipt-agent.sh" \
    CRC_SELFTEST="$T/receipt-closed.script" "$BIN" "$P" 2> "$T/receipt-closed.err"
expect "$T/receipt-closed.out" receipt "claims=tests:exit 101=12/1,lint:not run,build:checked ran=5 failed=1 rows=5 counts=12/1 installs=0:npm install left-pad last=bg:npm run dev"
expect "$T/receipt-closed.out" message "Copied the handoff, 21 lines"
rc_handoff=$(grep -m1 '^pasteboard: ' "$T/receipt-closed.out" | sed 's/^pasteboard: //' |
    sed -E 's/- [0-9a-f]{7,} \(/- <hash> (/')
rc_want='Receipt: 5 commands ran, 1 failed |  | Claims checked: 3 | - Said tests pass: exit 101 (12 passed, 1 failed) | - Said lint is clean: not run | - Said it builds: checked |  | Commits: 1 | - <hash> (unsigned) agent notes |  | Installs: 1 | - npm install left-pad: ok |  | Tests | - cargo test --offline: 12 passed, 1 failed |  | Files written: 1 | - notes.md |  | Failed commands: 1 | - exit 101: cargo test --offline'
[ "$rc_handoff" = "$rc_want" ] || failed "receipt-closed.out: handoff is [$rc_handoff], expected [$rc_want]"
