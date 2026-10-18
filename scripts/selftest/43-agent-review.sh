# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh review.

# ---- agent review sessions: the hook keeps the file, the tab says so ----------
# Every terminal gets its own CRC_SESSION_DIR. An agent's PreToolUse hook
# (`crc --hook pre`) keeps the file as it was before the write; its
# PostToolUse hook (`crc --hook post`) records the file as it left it,
# which Undo checks; its Notification hook (`crc --hook notify`) marks the
# tab like an OSC 9 does. The stand-in agent runs the hooks the way Claude
# Code would, around every write. The tab counts the session's files (Home's
# Terminals row carries the same title), and Source Control lists them
# under "Changed by agents", where a row opens the file's diff and its menu
# keeps or undoes the change (Keep and Undo themselves are unit tests in
# project::review). A file written by a plain shell command while the
# terminal works is listed too, with no copy from before.
# The terminal runs in the project folder, so the binary by its full path.
case "$BIN" in /*) hook_bin=$BIN ;; *) hook_bin=$PWD/$BIN ;; esac
mkdir -p "$T/reviewproj"
git -C "$T/reviewproj" init -q
git -C "$T/reviewproj" config user.name 'caio GUI test'
git -C "$T/reviewproj" config user.email 'test@example.invalid'
git -C "$T/reviewproj" config commit.gpgsign false
printf 'a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n' > "$T/reviewproj/notes.txt"
git -C "$T/reviewproj" add notes.txt
git -C "$T/reviewproj" commit -q -m 'start the notes'
cat > "$T/review-agent.sh" <<AGENT
#!/bin/sh
printf '{"cwd":"$T/reviewproj","tool_name":"Edit","tool_input":{"file_path":"notes.txt"}}' | "$hook_bin" --hook pre
printf 'a\nB\nc\nd\ne\nf\ng\nh\ni\nj\nk\nL\n' > "$T/reviewproj/notes.txt"
printf '{"cwd":"$T/reviewproj","tool_name":"Edit","tool_input":{"file_path":"notes.txt"}}' | "$hook_bin" --hook post
printf '{"cwd":"$T/reviewproj","tool_name":"Write","tool_input":{"file_path":"new.txt"}}' | "$hook_bin" --hook pre
printf 'made by the agent\n' > "$T/reviewproj/new.txt"
printf '{"cwd":"$T/reviewproj","tool_name":"Write","tool_input":{"file_path":"new.txt"}}' | "$hook_bin" --hook post
printf 'written by a shell command\n' > "$T/reviewproj/shell.txt"
printf '{"message":"Claude is waiting for your input"}' | "$hook_bin" --hook notify
printf 'done\n'
# Then a burst of writes to a file already under review, as an agent at
# work makes: 20 writes, one every 0.4 s, the last of 20 lines, each
# between its two hooks. The counts are read on a worker, one read at a
# time, so the burst costs the window nothing it has to wait for.
sleep 3
i=0
while [ \$i -lt 20 ]; do
    i=\$((i+1))
    k=0
    printf '{"cwd":"$T/reviewproj","tool_name":"Write","tool_input":{"file_path":"new.txt"}}' | "$hook_bin" --hook pre
    : > "$T/reviewproj/new.txt"
    while [ \$k -lt \$i ]; do
        k=\$((k+1))
        printf 'burst %s line %s\n' \$i \$k >> "$T/reviewproj/new.txt"
    done
    printf '{"cwd":"$T/reviewproj","tool_name":"Write","tool_input":{"file_path":"new.txt"}}' | "$hook_bin" --hook post
    sleep 0.4
done
sleep 30
AGENT
chmod +x "$T/review-agent.sh"
# Rows of the Source Control list, from 0: the Changes heading, notes.txt
# (tracked, changed), then the untracked new.txt and shell.txt; the agents'
# heading at 4 and its three rows in path order, new.txt at 5.
cat > "$T/review.script" <<SCRIPT
wait 500
key 8 cmd,shift C
wait 700
wait 700
wait 600
dump $T/review.out
key 5 cmd,opt g
wait 800
dump $T/review-git.out
SCRIPT
# The burst lands now: the display link runs between these waits, so the
# watcher's changes and the hooks' events are taken as they come.
for _ in $(seq 100); do echo 'wait 100'; done >> "$T/review.script"
cat >> "$T/review.script" <<SCRIPT
dump $T/review-burst.out
click @git.row.5
wait 600
dump $T/review-diff.out
quit
SCRIPT
CRC_TERMINAL_SHELL=/bin/sh CRC_CLAUDE_COMMAND="$T/review-agent.sh" \
    CRC_SELFTEST="$T/review.script" "$BIN" "$T/reviewproj" 2> "$T/review.err"
expect "$T/review.out" terminals "waiting: Claude is waiting for your input review=3"
# Source Control: the repository's three changes, and the agent's three
# files under "Changed by agents", new.txt first.
expect "$T/review-git.out" git_changes "3"
expect "$T/review-git.out" git_agents "3"
expect "$T/review-git.out" git_agents_first "new.txt"
expect "$T/review-git.out" git_agent_rows "3"
# The shell-written file is listed, not touched.
[ "$(cat "$T/reviewproj/shell.txt")" = "written by a shell command" ] \
    || failed "review: shell.txt changed"
# The burst changed nothing counted.
expect "$T/review-burst.out" git_agents "3"
grep -q '^terminals: .*review=3$' "$T/review-burst.out" \
    || failed "review-burst.out: the tab lost the count during the burst: $(grep -m1 '^terminals:' "$T/review-burst.out")"
# An agent row opens the file's working-tree diff, as a Changes row does.
expect "$T/review-diff.out" git_diff "true"
grep -q '^tabs: .*new.txt (Working tree)' "$T/review-diff.out" \
    || failed "review-diff.out: no diff tab for new.txt: $(grep -m1 '^tabs:' "$T/review-diff.out")"
