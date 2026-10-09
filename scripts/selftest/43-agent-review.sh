# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh review.

# ---- agent review sessions: the hook keeps the file, the tab says so ----------
# Every terminal gets its own CRC_SESSION_DIR. An agent's PreToolUse hook
# (`crc --hook pre`) keeps the file as it was before the write; its
# Notification hook (`crc --hook notify`) marks the tab like an OSC 9 does.
# The stand-in agent runs both hooks the way Claude Code would.
# The terminal runs in the project folder, so the binary by its full path.
case "$BIN" in /*) hook_bin=$BIN ;; *) hook_bin=$PWD/$BIN ;; esac
mkdir -p "$T/reviewproj"
printf 'as it was\n' > "$T/reviewproj/notes.txt"
cat > "$T/review-agent.sh" <<AGENT
#!/bin/sh
printf '{"cwd":"$T/reviewproj","tool_name":"Edit","tool_input":{"file_path":"notes.txt"}}' | "$hook_bin" --hook pre
printf 'the agent wrote this\n' > "$T/reviewproj/notes.txt"
printf '{"message":"Claude is waiting for your input"}' | "$hook_bin" --hook notify
printf 'done\n'
sleep 30
AGENT
chmod +x "$T/review-agent.sh"
cat > "$T/review.script" <<SCRIPT
wait 500
key 8 cmd,shift C
wait 700
wait 700
wait 600
dump $T/review.out
quit
SCRIPT
CRC_TERMINAL_SHELL=/bin/sh CRC_CLAUDE_COMMAND="$T/review-agent.sh" \
    CRC_SELFTEST="$T/review.script" "$BIN" "$T/reviewproj" 2> "$T/review.err"
expect "$T/review.out" terminals "waiting: Claude is waiting for your input review=1"
kept=$(ls "$HOME/Library/Application Support/crc/sessions"/*/files/*.path 2>/dev/null | head -1)
if [ -z "$kept" ]; then
    failed "review: no checkpoint in the session folder"
elif [ "$(cat "${kept%.path}")" != "as it was" ]; then
    failed "review: the checkpoint is not the file as it was: $(cat "${kept%.path}")"
fi
