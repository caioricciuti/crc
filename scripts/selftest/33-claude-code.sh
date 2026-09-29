# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh claude-code.

# ---- Claude Code ------------------------------------------------------------
# scripts/fake-claude.py plays claude: it finds the lock file, is refused
# with a wrong token, runs the MCP handshake and the read-only tools, waits
# for the selection, then proposes two edits. The first is accepted with the
# button, which claude answers by writing the file; the second is rejected
# with Escape and must leave its file alone.
mkdir -p "$T/claudeproj"
printf 'fn main() {}\n' > "$T/claudeproj/main.txt"
printf 'one\ntwo\nthree\n' > "$T/claudeproj/accept.txt"
printf 'unchanged\n' > "$T/claudeproj/reject.txt"
python3 scripts/fake-claude.py "$CLAUDE_CONFIG_DIR" "$T/claudeproj" "$T/claude.json" 2> "$T/fake-claude.err" &
FAKE_CLAUDE=$!
cat > "$T/claude.script" <<SCRIPT
wait 500
wait 500
wait 500
wait 500
dump $T/claude-review.out
click @review.accept
wait 300
wait 300
wait 300
wait 300
dump $T/claude-second.out
key 53
wait 300
wait 300
wait 300
dump $T/claude-done.out
quit
SCRIPT
CRC_SELFTEST="$T/claude.script" "$BIN" "$T/claudeproj/main.txt" 2> "$T/claude.err"
wait "$FAKE_CLAUDE" || true
if [ -s "$T/fake-claude.err" ]; then
    echo "FAIL: fake-claude.py wrote to stderr:"
    cat "$T/fake-claude.err"
    fail=1
fi
expect_claude "not j['errors']" "fake claude reported errors: $(cat "$T/claude.json" 2>/dev/null)"
expect_claude "j['lock_mode'] == '0o600'" "lock file is not private"
expect_claude "j['lock_ide'] == 'crc' and j['lock_transport'] == 'ws'" "lock file names"
expect_claude "j['wrong_token'].startswith('HTTP/1.1 401')" "a wrong token was not refused"
expect_claude "j['right_token'].startswith('HTTP/1.1 101')" "the right token was not accepted"
expect_claude "j['protocol'] == '2024-11-05' and j['server'] == 'crc'" "initialize"
expect_claude "'openDiff' in j['tools'] and 'getDiagnostics' in j['tools']" "tools/list"
expect_claude "j['editors'] == ['main.txt']" "getOpenEditors"
expect_claude "j['selection_file'] == 'main.txt' and j['selection_empty']" "selection_changed"
expect_claude "j['selection_start'] == {'line': 0, 'character': 0}" "selection position"
expect_claude "j['accept_reply'] == ['FILE_SAVED', 'one\nTWO\nthree\n']" "accept reply"
expect_claude "j['accept_close'] == ['TAB_CLOSED']" "close_tab after accept"
expect_claude "j['reject_reply'] == ['DIFF_REJECTED', 'review-reject']" "reject reply"
expect_claude "j['reject_close'] == ['TAB_CLOSED']" "close_tab after reject"
expect "$T/claude-review.out" claude "connected=true reviews=1 pending=1"
expect "$T/claude-review.out" tabs "main.txt | ✻ accept.txt"
expect "$T/claude-second.out" claude "connected=true reviews=1 pending=1"
expect "$T/claude-second.out" tabs "main.txt | ✻ reject.txt"
expect "$T/claude-done.out" tabs "main.txt"
if [ "$(cat "$T/claudeproj/accept.txt")" != "$(printf 'one\nTWO\nthree')" ]; then
    echo "FAIL: the accepted change was not written"
    fail=1
fi
if [ "$(cat "$T/claudeproj/reject.txt")" != "unchanged" ]; then
    echo "FAIL: the rejected change touched its file"
    fail=1
fi
if ls "$CLAUDE_CONFIG_DIR/ide/"*.lock >/dev/null 2>&1; then
    echo "FAIL: a lock file outlived its app: $(ls "$CLAUDE_CONFIG_DIR/ide/")"
    fail=1
fi
