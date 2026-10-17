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
# Then, with the page open, a burst of writes to a file already under
# review, as an agent at work makes: 20 writes, one every 0.4 s, the
# last of 20 lines.
sleep 3
i=0
while [ \$i -lt 20 ]; do
    i=\$((i+1))
    k=0
    : > "$T/reviewproj/new.txt"
    while [ \$k -lt \$i ]; do
        k=\$((k+1))
        printf 'burst %s line %s\n' \$i \$k >> "$T/reviewproj/new.txt"
    done
    sleep 0.4
done
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
SCRIPT
# The burst lands while the page is open: the display link runs between
# these waits, so the watcher's changes are taken as they come; short
# waits, so they are taken soon after each write.
for _ in $(seq 90); do echo 'wait 100'; done >> "$T/review.script"
cat >> "$T/review.script" <<SCRIPT
dump $T/review-burst.out
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
# The burst changed nothing Home counts; the page read its file again
# once the writes paused, on a worker: a read for opening, one for the
# burst. Reading every session's files on the main thread on each watcher
# event kept the window at 100% CPU.
expect "$T/review-burst.out" review_page "open sessions=1 files=3 hunks=1 shown=new.txt confirm=false targets=10"
expect "$T/review-open.out" review_diff_lines "2"
expect "$T/review-burst.out" review_diff_lines "21"
loads=$(grep -m1 '^review_loads: ' "$T/review-burst.out" | sed 's/^review_loads: //')
[ -n "$loads" ] && [ "$loads" -le 3 ] \
    || failed "review-burst.out: the page was read $loads times for a burst of 20 writes"
echo "review: the page was read $loads times for opening it and a burst of 20 writes"
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

# ---- a big session opens at once: read on a worker, skeleton meanwhile ---------
# 30 files of 200 KB, each changed on every eighth line by the agent. The
# page used to read and diff them all on the main thread when it opened,
# which froze the window; now the click returns at once and the page is
# drawn loading until the worker's read lands.
# The first launch's terminal is still held: taken back, or the next
# launch shows it instead of starting this agent.
pkill -f -- "--hold $T/home/" 2>/dev/null || true
find "$T/home" -type d -path '*/crc/held' -prune -exec rm -rf {} + 2>/dev/null || true
mkdir -p "$T/reviewbig"
for n in $(seq 30); do
    awk 'BEGIN { for (i = 0; i < 5500; i++) printf "line %06d of the big fixture\n", i }' \
        > "$T/reviewbig/f$n.txt"
done
cat > "$T/review-big-agent.sh" <<AGENT
#!/bin/sh
for n in \$(seq 30); do
    printf '{"cwd":"$T/reviewbig","tool_name":"Edit","tool_input":{"file_path":"f%s.txt"}}' "\$n" | "$hook_bin" --hook pre
    awk 'NR % 8 == 0 { sub("line", "row") } 1' "$T/reviewbig/f\$n.txt" > "$T/reviewbig/f\$n.tmp"
    mv "$T/reviewbig/f\$n.tmp" "$T/reviewbig/f\$n.txt"
done
printf 'done\n'
sleep 30
AGENT
chmod +x "$T/review-big-agent.sh"
cat > "$T/review-big.script" <<SCRIPT
wait 500
key 8 cmd,shift C
wait 700
wait 700
wait 700
wait 700
dump $T/review-big-home.out
click @home.review
dump $T/review-big-open.out
wait 700
wait 700
wait 700
dump $T/review-big-loaded.out
quit
SCRIPT
CRC_TERMINAL_SHELL=/bin/sh CRC_CLAUDE_COMMAND="$T/review-big-agent.sh" \
    CRC_SELFTEST="$T/review-big.script" "$BIN" "$T/reviewbig" 2> "$T/review-big.err"
grep -q 'review=30' "$T/review-big-home.out" \
    || failed "review-big-home.out: the agent's 30 files are not all counted: $(grep '^terminals:' "$T/review-big-home.out")"
# Right after the click: open, nothing read yet, the Close button its one target.
expect "$T/review-big-open.out" review_page "open sessions=0 files=0 hunks=0 shown=none confirm=false targets=1 loading"
expect "$T/review-big-open.out" review_loading "true"
expect "$T/review-big-loaded.out" review_loading "false"
grep -q '^review_page: open sessions=1 files=30 ' "$T/review-big-loaded.out" \
    || failed "review-big-loaded.out: $(grep '^review_page:' "$T/review-big-loaded.out")"
echo "review: one read of the 30-file session took $(grep -m1 '^review_load_ms: ' "$T/review-big-loaded.out" | sed 's/^review_load_ms: //') ms on its worker"
