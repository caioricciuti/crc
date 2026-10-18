# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh agent-changes.

# ---- Source Control lists what agents changed ---------------------------------
# A stand-in agent in a crc terminal edits a committed file and creates a
# new one, each between the two hooks. Source Control then has a last
# group, "Changed by agents", with the session's two files inside the
# repository; clicking one opens its diff as the Changes row does, and
# the heading folds the group.
case "$BIN" in /*) hook_bin=$BIN ;; *) hook_bin=$PWD/$BIN ;; esac
mkdir -p "$T/agproj"
git -C "$T/agproj" init -q
git -C "$T/agproj" config user.name 'caio GUI test'
git -C "$T/agproj" config user.email 'test@example.invalid'
git -C "$T/agproj" config commit.gpgsign false
printf 'a\nb\nc\n' > "$T/agproj/notes.txt"
git -C "$T/agproj" add notes.txt
git -C "$T/agproj" commit -q -m 'start the notes'
cat > "$T/agents-agent.sh" <<AGENT
#!/bin/sh
printf '{"cwd":"$T/agproj","tool_name":"Edit","tool_input":{"file_path":"notes.txt"}}' | "$hook_bin" --hook pre
printf 'a\nB\nc\n' > "$T/agproj/notes.txt"
printf '{"cwd":"$T/agproj","tool_name":"Edit","tool_input":{"file_path":"notes.txt"}}' | "$hook_bin" --hook post
printf '{"cwd":"$T/agproj","tool_name":"Write","tool_input":{"file_path":"new.txt"}}' | "$hook_bin" --hook pre
printf 'made by the agent\n' > "$T/agproj/new.txt"
printf '{"cwd":"$T/agproj","tool_name":"Write","tool_input":{"file_path":"new.txt"}}' | "$hook_bin" --hook post
printf 'done\n'
sleep 30
AGENT
chmod +x "$T/agents-agent.sh"
# Rows of the list, from 0: the Changes heading, notes.txt and new.txt
# (Git lists tracked changes before untracked files), then the agents'
# heading at 3 and its two rows in path order, new.txt at 4 and notes.txt
# at 5.
cat > "$T/agents.script" <<SCRIPT
wait 500
key 8 cmd,shift C
wait 700
wait 700
wait 700
key 5 cmd,opt g
wait 800
dump $T/agents.out
frame $T/agents-panel.bmp
click @git.row.4
wait 600
dump $T/agents-diff.out
click @git.row.3
wait 200
dump $T/agents-folded.out
quit
SCRIPT
CRC_TERMINAL_SHELL=/bin/sh CRC_CLAUDE_COMMAND="$T/agents-agent.sh" \
    CRC_SELFTEST="$T/agents.script" "$BIN" "$T/agproj" 2> "$T/agents.err"
expect "$T/agents.out" git_changes "2"
expect "$T/agents.out" git_agents "2"
expect "$T/agents.out" git_agents_first "new.txt"
expect "$T/agents.out" git_agent_rows "2"
grep -q '^terminals: .*review=2$' "$T/agents.out" \
    || failed "agents.out: the tab does not count the agent's two files: $(grep -m1 '^terminals:' "$T/agents.out")"
# The first agent row opens the file's working-tree diff, as a Changes
# row does.
expect "$T/agents-diff.out" git_diff "true"
grep -q '^tabs: .*new.txt (Working tree)' "$T/agents-diff.out" \
    || failed "agents-diff.out: no diff tab for new.txt: $(grep -m1 '^tabs:' "$T/agents-diff.out")"
# The heading folds the group; the files stay counted.
expect "$T/agents-folded.out" git_agents "2"
expect "$T/agents-folded.out" git_agent_rows "0"
