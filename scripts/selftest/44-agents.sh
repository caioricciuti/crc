# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh agents.

# ---- Agents mode: tasks are worktrees, each with its own sessions -----------
# Cmd-Shift-A gives the terminal the editor column and the sidebar to the tasks,
# starting Claude in the repository when nothing runs. New Task makes a
# branch in a worktree of its own (under CRC_WORKTREES here) and starts
# Claude there. The stand-in agent ends its turn with the Stop hook, which
# marks its session as waiting. Control-2 shows the second task; Cmd-Shift-A
# again gives the column back to the editor.
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
cat > "$T/agents.script" <<SCRIPT
wait 500
key 0 cmd,shift A
wait 700
wait 700
wait 600
dump $T/agents-on.out
key 45 cmd,shift N
wait 200
text try-it
key 36
wait 700
wait 700
wait 700
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
key 0 cmd,shift A
wait 400
dump $T/agents-off.out
quit
SCRIPT
CRC_TERMINAL_SHELL=/bin/sh CRC_CLAUDE_COMMAND="$T/agents-agent.sh" \
    CRC_WORKTREES="$T/worktrees" \
    CRC_SELFTEST="$T/agents.script" "$BIN" "$T/agentsproj" 2> "$T/agents.err"
expect "$T/agents-on.out" agents "on tasks=main selected=main sessions=*main pair=- op=-"
expect "$T/agents-on.out" terminals "waiting: finished"
expect "$T/agents-on.out" teams "0: [x] Write tests (tester); [ ] Fix login (fixer) idle=tester"
expect "$T/agents-task.out" agents "on tasks=main,try-it selected=try-it sessions=main,*try-it pair=- op=-"
ls -d "$T"/worktrees/agentsproj-*/try-it >/dev/null 2>&1 \
    || failed "agents: no worktree for try-it under $T/worktrees"
testgit "$T/agentsproj" rev-parse -q --verify refs/heads/try-it >/dev/null \
    || failed "agents: no try-it branch: $(testgit "$T/agentsproj" branch)"
expect_terminal "$T/agents-task.out" "agent ready in try-it"
# A task's row shows its waiting session, with the keyboard.
expect "$T/agents-main.out" agents "on tasks=main,try-it selected=main sessions=*main,try-it pair=- op=-"
expect "$T/agents-jump.out" agents "on tasks=main,try-it selected=try-it sessions=main,*try-it pair=- op=-"
# Cmd-\ puts a new shell in the task beside its agent; Cmd-Option-]
# moves the keyboard across; Cmd-Option-W goes back to one, and the shell
# keeps its tab.
expect "$T/agents-split.out" agents "on tasks=main,try-it selected=try-it sessions=main,try-it,*try-it pair=1+2 op=-"
expect "$T/agents-other.out" agents "on tasks=main,try-it selected=try-it sessions=main,*try-it,try-it pair=1+2 op=-"
expect "$T/agents-unsplit.out" agents "on tasks=main,try-it selected=try-it sessions=main,*try-it,try-it pair=- op=-"
# Back in the editor, the sessions keep running.
expect "$T/agents-off.out" agents "off tasks=main,try-it selected=try-it sessions=main,*try-it,try-it pair=- op=-"
grep -q '^terminals: waiting: finished|waiting: finished|' "$T/agents-off.out" \
    || failed "agents-off.out: $(grep '^terminals:' "$T/agents-off.out")"
testgit "$T/agentsproj" worktree remove --force "$(ls -d "$T"/worktrees/agentsproj-*/try-it)" 2>/dev/null || true
