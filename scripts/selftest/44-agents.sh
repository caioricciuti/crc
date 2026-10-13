# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh agents.

# ---- Agents mode: workspaces, each with its own sessions --------------------
# Cmd-Shift-A gives the terminal the editor column and the sidebar to the
# workspaces, starting a shell in the repository when nothing runs; with no
# workspace yet, one is made on the folder (agentsproj) and its repository's
# worktrees join the list. New Workspace offers a folder workspace on a
# folder you choose (a test instance takes the selected one's), one on the
# selected workspace's folder, and a task on a branch of its repository in
# a worktree of its own (under CRC_WORKTREES here); each starts a shell.
# The list is one for the app, kept under HOME: this scenario has its own. The agent is typed into the shell, as `claude` would be:
# here a stand-in on PATH that ends its turn with the Stop hook, which
# marks its session as waiting. Control-2 shows the second workspace; Cmd-T
# starts a shell in the selected one; a double-click under the list makes a
# workspace with a shell; Cmd-Shift-A again gives the column back to the
# editor. No session ever runs Claude on its own (claude=0).
case "$BIN" in /*) hook_bin=$BIN ;; *) hook_bin=$PWD/$BIN ;; esac
mkdir -p "$T/agentsproj"
printf 'one\n' > "$T/agentsproj/a.txt"
testgit "$T/agentsproj" init -q -b main
testgit "$T/agentsproj" add a.txt
testgit "$T/agentsproj" commit -qm one
cat > "$T/agents-agent.sh" <<AGENT
#!/bin/sh
printf 'agent ready in %s\n' "\$(basename "\$PWD")"
printf '{}' | "$hook_bin" --hook stop
# Its team: two tasks made, one finished, one teammate idle after.
printf '{"task_id":"1","task_subject":"Write tests","teammate_name":"tester"}' | "$hook_bin" --hook task-created
printf '{"task_id":"2","task_subject":"Fix login","teammate_name":"fixer"}' | "$hook_bin" --hook task-created
printf '{"task_id":"1","task_subject":"Write tests","teammate_name":"tester"}' | "$hook_bin" --hook task-completed
printf '{"teammate_name":"tester"}' | "$hook_bin" --hook teammate-idle
sleep 30
AGENT
chmod +x "$T/agents-agent.sh"
mkdir -p "$T/agentsbin"
cp "$T/agents-agent.sh" "$T/agentsbin/agentstub"
cat > "$T/agents.script" <<SCRIPT
wait 500
key 0 cmd,shift A
wait 700
text agentstub
key 36
wait 700
wait 700
wait 600
dump $T/agents-on.out
key 45 cmd,shift N
wait 200
text try-it
key 125
key 125
key 36
wait 700
wait 700
wait 700
text agentstub
key 36
wait 600
dump $T/agents-task.out
click @agents.task.0
wait 400
dump $T/agents-main.out
key 19 ctrl 2
wait 400
dump $T/agents-jump.out
key 42 cmd \\
wait 700
wait 300
dump $T/agents-split.out
key 30 cmd,opt ]
wait 200
dump $T/agents-other.out
key 13 cmd,opt w
wait 200
dump $T/agents-unsplit.out
key 19 cmd
wait 300
text cd $T/elsewhere
key 36
wait 700
wait 700
wait 500
dump $T/agents-left.out
text cd -
key 36
wait 700
wait 700
wait 500
dump $T/agents-back.out
key 45 cmd,shift N
wait 200
text notes
key 36
wait 700
text agentstub
key 36
wait 700
dump $T/agents-folder.out
key 17 cmd t
wait 700
dump $T/agents-cmdt.out
click @agents.blank
wait 500
dump $T/agents-single.out
click @agents.blank 2
wait 700
dump $T/agents-blank.out
key 0 cmd,shift A
wait 400
dump $T/agents-off.out
quit
SCRIPT
mkdir -p "$T/home/agents" "$T/home/agents-plain" "$T/elsewhere"
HOME="$T/home/agents" PATH="$T/agentsbin:$PATH" CRC_TERMINAL_SHELL=/bin/sh \
    CRC_CLAUDE_COMMAND="$T/agents-agent.sh" CRC_WORKTREES="$T/worktrees" \
    CRC_SELFTEST="$T/agents.script" "$BIN" "$T/agentsproj" 2> "$T/agents.err"
expect "$T/agents-on.out" agents "on tasks=agentsproj selected=agentsproj sessions=*agentsproj claude=0 pair=- op=-"
expect "$T/agents-on.out" terminals "waiting: finished"
expect "$T/agents-on.out" teams "0: [x] Write tests (tester); [ ] Fix login (fixer) idle=tester"
expect "$T/agents-task.out" agents "on tasks=agentsproj,try-it selected=try-it sessions=agentsproj,*try-it claude=0 pair=- op=-"
ls -d "$T"/worktrees/agentsproj-*/try-it >/dev/null 2>&1 \
    || failed "agents: no worktree for try-it under $T/worktrees"
testgit "$T/agentsproj" rev-parse -q --verify refs/heads/try-it >/dev/null \
    || failed "agents: no try-it branch: $(testgit "$T/agentsproj" branch)"
expect_terminal "$T/agents-task.out" "agent ready in try-it"
# The worktree's row sits under the workspace holding its repository.
expect "$T/agents-task.out" agents_tree "agentsproj,>try-it"
# A task's row shows its waiting session, with the keyboard.
expect "$T/agents-main.out" agents "on tasks=agentsproj,try-it selected=agentsproj sessions=*agentsproj,try-it claude=0 pair=- op=-"
expect "$T/agents-jump.out" agents "on tasks=agentsproj,try-it selected=try-it sessions=agentsproj,*try-it claude=0 pair=- op=-"
# Cmd-\ puts a new shell in the task beside its agent; Cmd-Option-]
# moves the keyboard across; Cmd-Option-W goes back to one, and the shell
# keeps its tab.
expect "$T/agents-split.out" agents "on tasks=agentsproj,try-it selected=try-it sessions=agentsproj,try-it,*try-it claude=0 pair=1+2 op=-"
expect "$T/agents-other.out" agents "on tasks=agentsproj,try-it selected=try-it sessions=agentsproj,*try-it,try-it claude=0 pair=1+2 op=-"
expect "$T/agents-unsplit.out" agents "on tasks=agentsproj,try-it selected=try-it sessions=agentsproj,*try-it,try-it claude=0 pair=- op=-"
# The shell beside the agent cds out of the worktree to a folder nothing
# holds: a workspace is made there and has it. Back in the worktree, it is
# try-it's again, and the workspace it left empty goes.
expect "$T/agents-left.out" agents "on tasks=agentsproj,try-it,elsewhere selected=elsewhere sessions=agentsproj,try-it,*elsewhere claude=0 pair=- op=-"
expect "$T/agents-back.out" agents "on tasks=agentsproj,try-it selected=try-it sessions=agentsproj,try-it,*try-it claude=0 pair=- op=-"
grep -q '^inspector: branch=try-it ' "$T/agents-back.out" \
    || failed "agents-back.out: $(grep '^inspector:' "$T/agents-back.out")"
# A folder workspace on the selected one's folder (try-it's repository),
# with a shell in it; Cmd-T adds a shell to it. It is kept.
# Two workspaces on the repository: the worktree sits under the first.
expect "$T/agents-folder.out" agents_tree "agentsproj,>try-it,notes"
expect "$T/agents-folder.out" agents "on tasks=agentsproj,try-it,notes selected=notes sessions=agentsproj,try-it,try-it,*notes claude=0 pair=- op=-"
expect_terminal "$T/agents-folder.out" "agent ready in agentsproj"
expect "$T/agents-cmdt.out" agents "on tasks=agentsproj,try-it,notes selected=notes sessions=agentsproj,try-it,try-it,notes,*notes claude=0 pair=- op=-"
grep -q "^notes	" "$T"/home/agents/Library/Application\ Support/crc/agents/workspaces 2>/dev/null \
    || failed "agents: the notes workspace was not kept under Application Support/crc/agents"
# One click under the list does nothing; a double-click makes a workspace
# on the selected one's folder, named after it, with a shell.
expect "$T/agents-single.out" agents "on tasks=agentsproj,try-it,notes selected=notes sessions=agentsproj,try-it,try-it,notes,*notes claude=0 pair=- op=-"
expect "$T/agents-blank.out" agents "on tasks=agentsproj,try-it,notes,agentsproj 2 selected=agentsproj 2 sessions=agentsproj,try-it,try-it,notes,notes,*agentsproj 2 claude=0 pair=- op=-"
# Back in the editor, the sessions keep running.
expect "$T/agents-off.out" agents "off tasks=agentsproj,try-it,notes,agentsproj 2 selected=agentsproj 2 sessions=agentsproj,try-it,try-it,notes,notes,*agentsproj 2 claude=0 pair=- op=-"
grep -q '^terminals: waiting: finished|waiting: finished|' "$T/agents-off.out" \
    || failed "agents-off.out: $(grep '^terminals:' "$T/agents-off.out")"
testgit "$T/agentsproj" worktree remove --force "$(ls -d "$T"/worktrees/agentsproj-*/try-it)" 2>/dev/null || true

# ---- Agents mode outside Git: a terminal never runs without a workspace ----
# A plain folder has no worktrees, so the first shell makes a workspace on
# the folder, named after it; Cmd-T adds a shell to it.
mkdir -p "$T/plainproj"
printf 'x\n' > "$T/plainproj/x.txt"
# A repository beside it, for the shell to cd into: the workspace and the
# inspector follow the terminal showing.
mkdir -p "$T/movedrepo"
printf 'x\n' > "$T/movedrepo/x.txt"
testgit "$T/movedrepo" init -q -b main
testgit "$T/movedrepo" add x.txt
testgit "$T/movedrepo" commit -qm one
cat > "$T/agents-plain.script" <<SCRIPT
wait 500
key 0 cmd,shift A
wait 700
wait 700
dump $T/agents-plain.out
key 17 cmd t
wait 700
dump $T/agents-plain-cmdt.out
text cd ../movedrepo
key 36
wait 700
wait 700
wait 500
dump $T/agents-moved.out
key 45 cmd n
wait 200
text other
key 36
wait 700
wait 300
dump $T/agents-other-made.out
key 18 ctrl
wait 400
key 18 cmd
wait 300
dump $T/agents-cmd1.out
key 19 cmd
wait 300
dump $T/agents-cmd2.out
text exit
key 36
wait 700
wait 300
dump $T/agents-one-left.out
text exit
key 36
wait 700
wait 300
dump $T/agents-closed.out
key 0 cmd,shift A
wait 700
dump $T/agents-plain-off.out
key 50 ctrl \`
wait 500
key 0 cmd,shift A
wait 700
key 0 cmd,shift A
wait 700
dump $T/agents-plain-off2.out
quit
SCRIPT
HOME="$T/home/agents-plain" CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/agents-plain.script" "$BIN" "$T/plainproj" \
    2> "$T/agents-plain.err"
expect "$T/agents-plain.out" agents "on tasks=plainproj selected=plainproj sessions=*plainproj claude=0 pair=- op=-"
expect "$T/agents-plain-cmdt.out" agents "on tasks=plainproj selected=plainproj sessions=plainproj,*plainproj claude=0 pair=- op=-"
# After `cd ../movedrepo` in the second shell, the workspace (named by crc
# after its folder) is that folder now, and the inspector reads its Git.
expect "$T/agents-moved.out" agents "on tasks=movedrepo selected=movedrepo sessions=movedrepo,*movedrepo claude=0 pair=- op=-"
grep -q '^inspector: branch=main base=- ahead=0 behind=0 upstream=-' "$T/agents-moved.out" \
    || failed "agents-moved.out: $(grep '^inspector:' "$T/agents-moved.out")"
# A second workspace, then back to the first: Command-1 and 2 pick its
# terminals. The one showing exits: the next shown is the first
# workspace's other terminal, not the second workspace's that slid into
# its place. The last one exits and its workspace goes with it.
expect "$T/agents-other-made.out" agents "on tasks=movedrepo,other selected=other sessions=movedrepo,movedrepo,*other claude=0 pair=- op=-"
# The first terminal never left plainproj: showing it, the workspace is there.
expect "$T/agents-cmd1.out" agents "on tasks=plainproj,other selected=plainproj sessions=*plainproj,plainproj,other claude=0 pair=- op=-"
expect "$T/agents-cmd2.out" agents "on tasks=movedrepo,other selected=movedrepo sessions=movedrepo,*movedrepo,other claude=0 pair=- op=-"
# The one left is the plainproj shell: the workspace follows it there.
expect "$T/agents-one-left.out" agents "on tasks=plainproj,other selected=plainproj sessions=*plainproj,other claude=0 pair=- op=-"
expect "$T/agents-closed.out" agents "on tasks=other selected=other sessions=*other claude=0 pair=- op=-"
# Agents leads: back in the editor, the project is the selected
# workspace's folder (movedrepo), not the one the editor opened on.
grep -q '^home: .* repos=movedrepo:main:' "$T/agents-plain-off.out" \
    || failed "agents-plain-off.out: $(grep '^home:' "$T/agents-plain-off.out")"
# The editor never opens the terminal for you, even one it had open.
grep -q '^terminal: open=false ' "$T/agents-plain-off.out" \
    || failed "agents-plain-off.out: $(grep '^terminal:' "$T/agents-plain-off.out" | cut -c1-80)"
grep -q '^terminal: open=false ' "$T/agents-plain-off2.out" \
    || failed "agents-plain-off2.out: $(grep '^terminal:' "$T/agents-plain-off2.out" | cut -c1-80)"
