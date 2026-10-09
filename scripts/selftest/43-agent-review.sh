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
printf 'a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n' > "$T/reviewproj/notes.txt"
cat > "$T/review-agent.sh" <<AGENT
#!/bin/sh
printf '{"cwd":"$T/reviewproj","tool_name":"Edit","tool_input":{"file_path":"notes.txt"}}' | "$hook_bin" --hook pre
printf 'a\nB\nc\nd\ne\nf\ng\nh\ni\nj\nk\nL\n' > "$T/reviewproj/notes.txt"
printf '{"cwd":"$T/reviewproj","tool_name":"Write","tool_input":{"file_path":"new.txt"}}' | "$hook_bin" --hook pre
printf 'made by the agent\n' > "$T/reviewproj/new.txt"
printf 'written by a shell command\n' > "$T/reviewproj/shell.txt"
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
click @home.review
wait 300
dump $T/review-open.out
click @review_page.select.0.1
wait 200
click @review_page.keep.0
wait 300
dump $T/review-kept.out
click @review_page.undo.0
wait 300
dump $T/review-undone.out
click @review_page.select.0.0
wait 200
click @review_page.undo_file
wait 200
dump $T/review-confirm.out
click @review_page.undo_file
wait 300
dump $T/review-shell.out
click @review_page.keep_file
wait 300
dump $T/review-empty.out
quit
SCRIPT
CRC_TERMINAL_SHELL=/bin/sh CRC_CLAUDE_COMMAND="$T/review-agent.sh" \
    CRC_SELFTEST="$T/review.script" "$BIN" "$T/reviewproj" 2> "$T/review.err"
expect "$T/review.out" terminals "waiting: Claude is waiting for your input review=3"
# Home lists the session; its row opens the page, newest session first,
# files in path order: new.txt, then notes.txt.
expect "$T/review-open.out" review_page "open sessions=1 files=3 hunks=1 shown=new.txt confirm=false targets=10"
# notes.txt has two changes: Keep the first, Undo the second. The file
# keeps B and goes back to l, and with nothing left it leaves the review.
expect "$T/review-kept.out" review_page "open sessions=1 files=3 hunks=1 shown=notes.txt confirm=false targets=10"
grep -q '^message: Kept a change in notes.txt$' "$T/review-kept.out" \
    || failed "review-kept.out: $(grep '^message:' "$T/review-kept.out")"
expect "$T/review-undone.out" review_page "open sessions=1 files=2 hunks=1 shown=new.txt confirm=false targets=9"
[ "$(cat "$T/reviewproj/notes.txt")" = "$(printf 'a\nB\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl')" ] \
    || failed "review: notes.txt after Keep and Undo: $(cat "$T/reviewproj/notes.txt")"
# Undo file asks twice; for a file the agent created it removes it.
grep -q '^review_page: .* confirm=true ' "$T/review-confirm.out" \
    || failed "review-confirm.out: $(grep '^review_page:' "$T/review-confirm.out")"
[ -e "$T/reviewproj/new.txt" ] && failed "review: new.txt is still there after Undo file"
# A file written by a plain shell command, not the hook, is listed too,
# with no copy from before: Keep file only, no Undo and no hunk buttons.
expect "$T/review-shell.out" review_page "open sessions=1 files=1 hunks=1 shown=shell.txt confirm=false targets=5"
grep -q '^review_page: .*shown=shell.txt' "$T/review-shell.out" && [ "$(cat "$T/reviewproj/shell.txt")" = "written by a shell command" ] \
    || failed "review: shell.txt changed"
expect "$T/review-empty.out" review_page "open sessions=0 files=0 hunks=0 shown=none confirm=false targets=1"
