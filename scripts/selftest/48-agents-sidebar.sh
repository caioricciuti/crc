# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh sidebar.

# ---- The Agents list: Cmd-N, drag to reorder, rename in place, restore ----
# A plain folder. Cmd-N in Agents mode takes a new workspace's name, as
# cmux and supacode have it (a test instance takes the folder named as it
# beside the selected one's); each row shows its folder. A row dragged
# above another stays there; a double-click on a row names it in place.
# Then a reboot, played as crc quitting and its session helpers ending:
# the next launch starts the lost terminals again in their workspaces, and
# types `claude --continue` into the one that ran Claude (a stand-in here,
# on PATH as `claude`). Two workspaces on one folder: scenario 60.
S="$T/sideproj"
mkdir -p "$S" "$T/sidebin" "$T/beta" "$T/gamma"
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
frame $T/side-made.bmp
down @agents.task.2
wait 100
dragby 0 -70
wait 100
upby 0 -70
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
# The list is one for the app, kept under HOME: this scenario has its own.
mkdir -p "$T/home/sidebar"
HOME="$T/home/sidebar" PATH="$T/sidebin:$PATH" CRC_TERMINAL_SHELL=/bin/sh CRC_CLAUDE_COMMAND="$T/side-agent.sh" \
    CRC_SELFTEST="$T/side.script" "$BIN" "$S" 2> "$T/side.err"
expect "$T/side-made.out" agents "on tasks=sideproj,beta,gamma selected=gamma sessions=sideproj,beta,*gamma claude=0 pair=- op=-"
expect "$T/side-made.out" agents_folders "sideproj=$S,beta=$T/beta,gamma=$T/gamma"
[ -z "${SELFTEST_FRAMES:-}" ] || cp -f "$T/side-made.bmp" "$SELFTEST_FRAMES/"
# gamma dragged above beta stays there.
expect "$T/side-dragged.out" agents "on tasks=sideproj,gamma,beta selected=gamma sessions=sideproj,beta,*gamma claude=0 pair=- op=-"
# The order file keeps ids: gamma's is its key in the workspaces file.
side_kept="$T/home/sidebar/Library/Application Support/crc/agents"
gamma_key=$(awk -F'\t' '$1 == "gamma" { print $3 }' "$side_kept/workspaces" 2>/dev/null)
[ -n "$gamma_key" ] && grep -qF "$gamma_key" "$side_kept/order" 2>/dev/null \
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
HOME="$T/home/sidebar" PATH="$T/sidebin:$PATH" CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/side-restore.script" \
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

# Another reboot, and crc opened on a folder none of them was started in:
# the workspaces are the app's, so every one's terminals come back. (Not
# `claude --continue` again: the stand-in tab started as a shell, and this
# quit wrote nothing that says an agent ran in it.)
pkill -f -- "--hold $T/home/" 2>/dev/null || true
sleep 1
mkdir -p "$T/sideother"
printf 'y\n' > "$T/sideother/y.txt"
cat > "$T/side-other.script" <<SCRIPT
wait 500
key 0 cmd,shift A
wait 1500
dump $T/side-other.out
quit
SCRIPT
HOME="$T/home/sidebar" PATH="$T/sidebin:$PATH" CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/side-other.script" \
    "$BIN" "$T/sideother" 2> "$T/side-other.err"
case "$(grep '^agents:' "$T/side-other.out")" in
*"tasks=sideproj,gamma,notes "*"sessions=sideproj,notes,gamma,"*) ;;
*) failed "sidebar: from another folder, sessions are $(grep '^agents:' "$T/side-other.out")" ;;
esac
grep -q "^message: Started again 4 terminals" "$T/side-other.out" \
    || failed "sidebar: from another folder: $(grep '^message:' "$T/side-other.out")"

# ---- Worktrees under the workspace holding their repository ---------------
# A folder holding a repository (garden-log) and, beside it, one of its
# worktrees; another worktree lives elsewhere. Both rows sit under the
# folder's workspace, and the inspector has one card for the repository
# (the worktree beside it is not taken for a second repository) with a
# strip per worktree. Terminal on a strip starts a shell there; the chevron
# folds the worktrees away, which Control-1 to 9 then skip, and the fold
# is kept; the branch name selects a worktree. A file written in a worktree
# from a shell turns its row to writing. Review shows the branch against
# main; once main takes it, the strip says merged, its row folds away under
# a Merged line that opens to show it, and Clean up removes it
# (refused while it has uncommitted work) with its branch. SELFTEST_FRAMES, when set, is a folder
# to keep the frames in.
GD="$T/garden"
GL="$GD/garden-log"
mkdir -p "$GL" "$T/garden-wt"
printf 'beds\n' > "$GL/beds.txt"
testgit "$GL" init -q -b main
testgit "$GL" add beds.txt
testgit "$GL" commit -qm beds
testgit "$GL" worktree add -q -b fix/seed-dates "$T/garden-wt/seed-dates"
printf 'march\napril\nmay\n' > "$T/garden-wt/seed-dates/dates.txt"
testgit "$T/garden-wt/seed-dates" add dates.txt
testgit "$T/garden-wt/seed-dates" commit -qm dates
testgit "$GL" worktree add -q -b feat/compost-log "$GD/garden-log-compost"
cat > "$T/garden.script" <<SCRIPT
wait 500
key 0 cmd,shift A
wait 900
wait 700
wait 700
dump $T/garden.out
frame $T/garden-frame.bmp
click @inspector.worktree_terminal.1
wait 700
wait 300
dump $T/garden-terminal.out
text echo april > $T/garden-wt/seed-dates/notes.txt
key 36
wait 1200
dump $T/garden-writing.out
frame $T/garden-writing.bmp
key 18 ctrl 1
wait 400
click @agents.fold.0
wait 300
dump $T/garden-folded.out
key 19 ctrl 2
wait 300
dump $T/garden-ctrl2.out
click @agents.fold.0
wait 300
key 20 ctrl 3
wait 500
dump $T/garden-ctrl3.out
key 18 ctrl 1
wait 700
click @inspector.worktree_open.0
wait 500
dump $T/garden-open.out
key 18 ctrl 1
wait 400
text git -C $GL merge -q --ff-only fix/seed-dates
key 36
wait 700
click @inspector.refresh
wait 1200
dump $T/garden-merged.out
frame $T/garden-merged.bmp
click @agents.merged.0
wait 300
dump $T/garden-merged-open.out
frame $T/garden-merged-open.bmp
click @inspector.worktree_cleanup.0
wait 500
dump $T/garden-dirty.out
key 18 ctrl 1
wait 300
text rm $T/garden-wt/seed-dates/notes.txt
key 36
wait 500
click @inspector.worktree_cleanup.0
wait 1500
wait 1500
dump $T/garden-cleaned.out
key 18 ctrl 1
wait 300
click @agents.fold.0
wait 300
quit
SCRIPT
mkdir -p "$T/home/garden"
HOME="$T/home/garden" CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/garden.script" \
    "$BIN" "$GD" 2> "$T/garden.err"
[ -z "${SELFTEST_FRAMES:-}" ] || cp -f "$T/garden-frame.bmp" "$T/garden-writing.bmp" \
    "$T/garden-merged.bmp" "$T/garden-merged-open.bmp" "$SELFTEST_FRAMES/"
expect "$T/garden.out" agents_tree "garden,>fix/seed-dates,>feat/compost-log"
expect "$T/garden.out" inspector "branch=main base=- ahead=0 behind=0 upstream=- push=0 pull=0 dirty=0 changes=+0-0 tag=- buttons=push worktrees=fix/seed-dates:ahead=1:+3-0:files=1,feat/compost-log:ahead=0:+0-0:files=0"
expect "$T/garden-terminal.out" agents "on tasks=garden,fix/seed-dates,feat/compost-log selected=feat/compost-log sessions=garden,*feat/compost-log claude=0 pair=- op=-"
# Nothing written yet: each says its branch's last commit.
expect "$T/garden.out" agents_activity "fix/seed-dates=commit,feat/compost-log=commit"
# A file written in a worktree from a shell elsewhere (as a background
# agent run from another session would): its watcher sees it.
expect "$T/garden-writing.out" agents_activity "fix/seed-dates=writing,feat/compost-log=commit"
expect "$T/garden-folded.out" agents_tree "garden(+2)"
# The fold at the end is kept for the next launch (beside the merged
# group's, opened above).
grep -q "^fold[,	]" "$T/home/garden/Library/Application Support/crc/agents/order" 2>/dev/null \
    || failed "garden: the fold was not kept: $(cat "$T/home/garden/Library/Application Support/crc/agents/order" 2>/dev/null)"
# Folded, there is no second row for Control-2 to pick.
expect "$T/garden-ctrl2.out" agents "on tasks=garden,fix/seed-dates,feat/compost-log selected=garden sessions=*garden,feat/compost-log claude=0 pair=- op=-"
expect "$T/garden-ctrl3.out" agents_tree "garden,>fix/seed-dates,>feat/compost-log"
expect "$T/garden-ctrl3.out" agents "on tasks=garden,fix/seed-dates,feat/compost-log selected=feat/compost-log sessions=garden,*feat/compost-log claude=0 pair=- op=-"
case "$(grep '^agents:' "$T/garden-open.out")" in
*" selected=fix/seed-dates "*) ;;
*) failed "garden: Open did not select the worktree: $(grep '^agents:' "$T/garden-open.out")" ;;
esac
# main takes the branch: it is merged now. The other worktree, never
# committed to, is on main too but not done. The strips follow the list,
# where the merged one has moved last.
case "$(grep '^inspector:' "$T/garden-merged.out")" in
*"worktrees=feat/compost-log:ahead=0:+0-0:files=0,fix/seed-dates:ahead=0:+0-0:files=0:merged") ;;
*) failed "garden: merged strips are $(grep '^inspector:' "$T/garden-merged.out")" ;;
esac
# The merged one leaves the list for a folded line after the active ones;
# the line opens to show it.
expect "$T/garden-merged.out" agents_tree "garden,>feat/compost-log,~merged(+1)"
expect "$T/garden-merged-open.out" agents_tree "garden,>feat/compost-log,~merged,>fix/seed-dates"
# Clean up refuses while the worktree has a file Git would lose.
grep -q "^message: fix/seed-dates has uncommitted changes; commit or discard them first" "$T/garden-dirty.out" \
    || failed "garden: clean up of a dirty worktree: $(grep '^message:' "$T/garden-dirty.out")"
# Then it removes the worktree and, merged, its branch.
expect "$T/garden-cleaned.out" agents_tree "garden,>feat/compost-log"
grep -q "^message: Removed fix/seed-dates and its branch" "$T/garden-cleaned.out" \
    || failed "garden: clean up said $(grep '^message:' "$T/garden-cleaned.out")"
[ ! -e "$T/garden-wt/seed-dates" ] || failed "garden: the cleaned up worktree's folder is still there"
testgit "$GL" rev-parse --verify --quiet fix/seed-dates >/dev/null 2>&1 \
    && failed "garden: the merged branch was kept"
testgit "$GL" worktree remove --force "$T/garden-wt/seed-dates" 2>/dev/null || true
testgit "$GL" worktree remove --force "$GD/garden-log-compost" 2>/dev/null || true
