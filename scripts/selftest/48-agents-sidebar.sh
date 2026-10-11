# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh sidebar.

# ---- The Agents list: Cmd-N, drag to reorder, rename in place, restore ----
# A plain folder. Cmd-N in Agents mode takes a new workspace's name, as
# cmux and supacode have it; a row dragged above another stays there; a
# double-click on a row names it in place. Then a reboot, played as crc
# quitting and its session helpers ending: the next launch starts the lost
# terminals again in their workspaces, and types `claude --continue` into
# the one that ran Claude (a stand-in here, on PATH as `claude`).
S="$T/sideproj"
mkdir -p "$S" "$T/sidebin"
printf 'x\n' > "$S/x.txt"
printf '#!/bin/sh\necho "resumed $*"\nsleep 30\n' > "$T/sidebin/claude"
chmod +x "$T/sidebin/claude"
cat > "$T/side-agent.sh" <<'AGENT'
#!/bin/sh
echo "first claude run"
sleep 30
AGENT
chmod +x "$T/side-agent.sh"
cat > "$T/side.script" <<SCRIPT
wait 500
key 0 cmd,shift A
wait 900
key 45 cmd n
wait 200
text beta
key 36
wait 700
key 45 cmd n
wait 200
text gamma
key 36
wait 700
dump $T/side-made.out
down @agents.task.2
wait 100
dragby 0 -50
wait 100
upby 0 -50
wait 400
dump $T/side-dragged.out
click @agents.task.2 2
wait 300
text notes
key 36
wait 500
dump $T/side-renamed.out
key 8 cmd,shift c
wait 900
dump $T/side-claude.out
quit
SCRIPT
PATH="$T/sidebin:$PATH" CRC_TERMINAL_SHELL=/bin/sh CRC_CLAUDE_COMMAND="$T/side-agent.sh" \
    CRC_SELFTEST="$T/side.script" "$BIN" "$S" 2> "$T/side.err"
expect "$T/side-made.out" agents "on tasks=sideproj,beta,gamma selected=gamma sessions=sideproj,beta,*gamma claude=0 pair=- op=-"
# gamma dragged above beta stays there.
expect "$T/side-dragged.out" agents "on tasks=sideproj,gamma,beta selected=gamma sessions=sideproj,beta,*gamma claude=0 pair=- op=-"
grep -q "gamma" "$T"/home/Library/Application\ Support/crc/agents/*.order 2>/dev/null \
    || failed "sidebar: the dragged order was not kept"
# beta, now third, renamed in its row (a folder workspace takes the name).
# (The script's double-click is one event, so it does not select first.)
expect "$T/side-renamed.out" agents "on tasks=sideproj,gamma,notes selected=gamma sessions=sideproj,notes,*gamma claude=0 pair=- op=-"

# The reboot: the helpers end without a word (the self-test's quit writes
# nothing either, as a crash would not); each session's record kept its
# workspace as it changed. The next launch starts the terminals again.
pkill -f -- "--hold $T/home/" 2>/dev/null || true
sleep 1
cat > "$T/side-restore.script" <<SCRIPT
wait 500
key 0 cmd,shift A
wait 1500
dump $T/side-restored.out
quit
SCRIPT
PATH="$T/sidebin:$PATH" CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/side-restore.script" \
    "$BIN" "$S" 2> "$T/side-restore.err"
grep -q '^agents: on tasks=sideproj,gamma,notes .*sessions=[^ ]*sideproj[^ ]*notes[^ ]*gamma[^ ]*claude=0' "$T/side-restored.out" 2>/dev/null \
    || grep -q '^agents: on tasks=sideproj,gamma,notes ' "$T/side-restored.out" \
    || failed "sidebar: $(grep '^agents:' "$T/side-restored.out")"
case "$(grep '^agents:' "$T/side-restored.out")" in
*"sessions=sideproj,notes,gamma,"*) ;;
*) failed "sidebar: restored sessions are $(grep '^agents:' "$T/side-restored.out")" ;;
esac
grep -q "^message: Started again 4 terminals" "$T/side-restored.out" \
    || failed "sidebar: $(grep '^message:' "$T/side-restored.out")"
expect_terminal "$T/side-restored.out" "resumed --continue"
