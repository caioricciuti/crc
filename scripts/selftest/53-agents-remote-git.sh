# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh remote-git.

# ---- A remote workspace's Git is read over ssh ------------------------------
# A remote folder workspace lists the worktrees of the repositories on its
# host, as a local one does, read by ssh with BatchMode. The stand-in ssh
# (CRC_SSH_COMMAND) runs those reads with this Mac's shell, so the host is a
# fixture repository here: `fix`, a commit ahead, and `old`, merged into
# main and folded under the Merged line. Then the host stops answering (the
# stand-in exits 255, as ssh does): the rows stay, and the folder's row says
# it is unreachable. A worktree row's terminal is ssh into its folder there.
# The host answers again: the inspector shows the worktree's Git as a local
# one's, and Commit All, Merge main In and Push run on the host, Push to a
# bare origin; a second launch pulls what someone else pushed there. A
# third, its origin now named on GitHub, has the pull request of its
# worktree read by a stand-in `gh` here, told the repository; cleans up the
# merged worktree on the host; and makes a new task there, its terminal ssh
# into the new worktree.
RG="$T/rgit"
RGO="$T/rgit-origin.git"
RGH="$T/home/rgit"
RGS="$RGH/Library/Application Support/crc"
mkdir -p "$RG" "$T/rgitproj" "$RGS/agents"
printf 'one\n' > "$RG/a.txt"
printf 'one\n' > "$T/rgitproj/a.txt"
testgit "$RG" init -q -b main
# The app commits with the repository's own settings, never signing.
testgit "$RG" config user.name Tester
testgit "$RG" config user.email t@example.com
testgit "$RG" config commit.gpgsign false
testgit "$RG" config tag.gpgsign false
testgit "$RG" add a.txt
testgit "$RG" commit -qm one
git init -q --bare -b main "$RGO"
testgit "$RG" remote add origin "$RGO"
testgit "$RG" push -q -u origin main
testgit "$RG" worktree add -q -b fix "$T/rgit-wt/fix"
printf 'two\n' > "$T/rgit-wt/fix/b.txt"
testgit "$T/rgit-wt/fix" add b.txt
testgit "$T/rgit-wt/fix" commit -qm 'fix the thing'
testgit "$RG" worktree add -q -b old "$T/rgit-wt/old"
printf 'three\n' > "$T/rgit-wt/old/c.txt"
testgit "$T/rgit-wt/old" add c.txt
testgit "$T/rgit-wt/old" commit -qm old
testgit "$RG" merge -q --no-edit old
fix_path=$(git -C "$RG" worktree list --porcelain | sed -n 's/^worktree \(.*rgit-wt\/fix\)$/\1/p')
printf 'app\t%s\tws:rgit1\tmyhost\n' "$RG" > "$RGS/agents/workspaces"
cat > "$T/rgit-ssh.sh" <<STUB
#!/bin/sh
for last; do :; done
case " \$* " in
*BatchMode=yes*)
    printf '%s\n' "\$*" >> "$T/rgit-batch.log"
    # The line itself, out of the \`sh -c\` ssh hands the login shell.
    ( eval "set -- \$last"; printf '%s\n' "\$3" ) >> "$T/rgit-lines.log"
    [ "\$(cat "$T/rgit-down" 2>/dev/null)" = down ] && exit 255
    exec /bin/sh -c "\$last"
    ;;
esac
printf '%s\n' "\$@" >> "$T/rgit-ssh.log"
echo '==' >> "$T/rgit-ssh.log"
echo 'remote stub up'
sleep 30
STUB
chmod +x "$T/rgit-ssh.sh"
cat > "$T/rgit.script" <<SCRIPT
wait 500
key 0 cmd,shift A
idle 1500
wait 500
dump $T/rgit-on.out
write $T/rgit-down down
click @agents.refresh
idle 1500
wait 500
dump $T/rgit-down.out
click @agents.task.1
wait 700
wait 700
dump $T/rgit-term.out
write $T/rgit-down up
write $T/rgit-wt/fix/new.txt new
click @inspector.refresh
wait 700
wait 700
dump $T/rgit-dirty.out
click @inspector.commit
wait 200
text wip remote
key 36
wait 700
wait 700
dump $T/rgit-committed.out
click @inspector.update
wait 700
wait 700
dump $T/rgit-updated.out
click @inspector.push
wait 700
wait 700
wait 500
dump $T/rgit-pushed.out
quit
SCRIPT
HOME="$RGH" CRC_TERMINAL_SHELL=/bin/sh CRC_SSH_COMMAND="$T/rgit-ssh.sh" \
    CRC_SELFTEST="$T/rgit.script" "$BIN" "$T/rgitproj" 2> "$T/rgit.err"
expect "$T/rgit-on.out" agents "on tasks=app,fix,old,rgitproj selected=rgitproj sessions=*rgitproj claude=0 pair=- op=-"
expect "$T/rgit-on.out" agents_tree "app,>fix,~merged(+1),rgitproj"
expect "$T/rgit-on.out" agents_activity "fix=commit,old=commit"
# The host stops answering: the rows it had stay.
expect "$T/rgit-down.out" agents_tree "app(unreachable),>fix,~merged(+1),rgitproj"
# The worktree's terminal is ssh to its folder on the host.
expect "$T/rgit-term.out" agents "on tasks=app,fix,old,rgitproj selected=fix sessions=rgitproj,*fix claude=0 pair=- op=-"
expect "$T/rgit-term.out" inspector "remote=myhost:$fix_path error=host unreachable buttons="
expect_terminal "$T/rgit-term.out" "remote stub up"
q="'\"'\"'"
grep -qxF -- "sh -c 'cd ${q}$fix_path${q} && exec \"\${SHELL:-/bin/sh}\" -l'" "$T/rgit-ssh.log" 2>/dev/null \
    || failed "remote-git: the worktree's ssh did not go to $fix_path: $(tr '\n' ' ' < "$T/rgit-ssh.log" 2>/dev/null)"
# Every read asked nothing. (The shared connection's socket would not fit
# under this run's long temporary HOME, so it is not asked for here; the
# unit tests check its options.)
blog="$T/rgit-batch.log"
llog="$T/rgit-lines.log"
for arg in "BatchMode=yes" "ConnectTimeout=5" "-- myhost sh -c 'cd -P "; do
    grep -qF -- "$arg" "$blog" 2>/dev/null || failed "remote-git: no read over ssh had [$arg]"
done
for arg in "cd -P '$RG'" "'worktree' 'list' '--porcelain'" "'rev-list' '--count' 'main..fix'"; do
    grep -qF -- "$arg" "$llog" 2>/dev/null || failed "remote-git: no line read over ssh had [$arg]"
done

# The host is back: the worktree's card, read over ssh, with its new file.
expect "$T/rgit-dirty.out" inspector "remote=myhost:$fix_path branch=fix base=main ahead=1 behind=1 upstream=- push=0 pull=0 dirty=1 changes=+1-0 tag=- buttons=commit,update,push"
expect "$T/rgit-committed.out" inspector "remote=myhost:$fix_path branch=fix base=main ahead=2 behind=1 upstream=- push=0 pull=0 dirty=0 changes=+2-0 tag=- buttons=update,push"
testgit "$fix_path" log --format=%s | grep -x 'wip remote' >/dev/null \
    || failed "remote-git: Commit All did not commit on the host: $(testgit "$fix_path" log -3 --format=%s | tr '\n' ' ')"
[ -z "$(testgit "$fix_path" status --porcelain)" ] \
    || failed "remote-git: Commit All left changes: $(testgit "$fix_path" status --porcelain)"
# Merging main in brings its one commit: the change from it stays fix's own.
expect "$T/rgit-updated.out" inspector "remote=myhost:$fix_path branch=fix base=main ahead=3 behind=0 upstream=- push=0 pull=0 dirty=0 changes=+2-0 tag=- buttons=push"
expect "$T/rgit-pushed.out" inspector "remote=myhost:$fix_path branch=fix base=main ahead=3 behind=0 upstream=origin/fix push=0 pull=0 dirty=0 changes=+2-0 tag=- check=none buttons=pull"
[ "$(git -C "$RGO" rev-parse -q --verify refs/heads/fix)" = "$(testgit "$fix_path" rev-parse HEAD)" ] \
    || failed "remote-git: Push did not put fix on origin"
# The changes went over ssh without a shared connection of their own, the
# message one argument.
grep -qF -- "ControlMaster=no" "$blog" 2>/dev/null || failed "remote-git: no change over ssh had [ControlMaster=no]"
for arg in "'add' '--all' && git 'commit' '-q' '-m' 'wip remote'" "'merge' '--no-edit' 'main'" "'push' '--set-upstream' 'origin' 'fix'"; do
    grep -qF -- "$arg" "$llog" 2>/dev/null || failed "remote-git: no change over ssh had [$arg]"
done

# Someone else pushes to fix; the next launch pulls it on the host.
git clone -q -b fix "$RGO" "$T/rgit-other"
printf 'theirs\n' > "$T/rgit-other/t.txt"
testgit "$T/rgit-other" add t.txt
testgit "$T/rgit-other" commit -qm theirs
testgit "$T/rgit-other" push -q origin fix
testgit "$fix_path" fetch -q origin
cat > "$T/rgit-pull.script" <<SCRIPT
wait 500
key 0 cmd,shift A
idle 1500
wait 500
click @agents.task.1
wait 700
wait 700
dump $T/rgit-behind.out
click @inspector.pull
wait 700
wait 700
dump $T/rgit-pulled.out
quit
SCRIPT
HOME="$RGH" CRC_TERMINAL_SHELL=/bin/sh CRC_SSH_COMMAND="$T/rgit-ssh.sh" \
    CRC_SELFTEST="$T/rgit-pull.script" "$BIN" "$T/rgitproj" 2> "$T/rgit-pull.err"
expect "$T/rgit-behind.out" inspector "remote=myhost:$fix_path branch=fix base=main ahead=3 behind=0 upstream=origin/fix push=0 pull=1 dirty=0 changes=+2-0 tag=- check=none buttons=pull"
expect "$T/rgit-pulled.out" inspector "remote=myhost:$fix_path branch=fix base=main ahead=4 behind=0 upstream=origin/fix push=0 pull=0 dirty=0 changes=+3-0 tag=- check=none buttons=pull"
[ -f "$fix_path/t.txt" ] || failed "remote-git: Pull did not bring t.txt on the host"
grep -qF -- "'pull' '--ff-only'" "$llog" 2>/dev/null || failed "remote-git: no pull over ssh had [--ff-only]"

# Its origin named on GitHub (nothing is fetched or pushed from here on):
# the stand-in `gh` on PATH answers for fix, and logs how it was asked.
testgit "$RG" remote set-url origin git@github.com:me/rgit.git
mkdir -p "$T/rgitbin"
cat > "$T/rgitbin/gh" <<'GH'
#!/bin/sh
echo "$*" >> "$RGIT_GH_LOG"
[ "$1 $2" = "pr view" ] || exit 1
if [ "$3" = fix ]; then
    echo '{"number":21,"state":"OPEN","url":"https://github.com/me/rgit/pull/21","statusCheckRollup":[{"status":"IN_PROGRESS","conclusion":""}]}'
    exit 0
fi
echo "no pull requests found for branch \"$3\"" >&2
exit 1
GH
chmod +x "$T/rgitbin/gh"
: > "$T/rgit-gh.log"
cat > "$T/rgit-tasks.script" <<SCRIPT
wait 500
key 0 cmd,shift A
idle 2500
dump $T/rgit-prs.out
click @agents.task.0
wait 700
wait 700
click @inspector.worktree_cleanup.0
wait 700
wait 700
wait 700
dump $T/rgit-cleaned.out
key 45 cmd,shift N
wait 200
text try-it
key 125
key 36
wait 700
wait 700
wait 700
dump $T/rgit-task.out
quit
SCRIPT
HOME="$RGH" PATH="$T/rgitbin:$PATH" RGIT_GH_LOG="$T/rgit-gh.log" CRC_SELFTEST_GH=1 \
    CRC_SELFTEST_PR_EVERY=1 CRC_TERMINAL_SHELL=/bin/sh CRC_SSH_COMMAND="$T/rgit-ssh.sh" \
    CRC_SELFTEST="$T/rgit-tasks.script" "$BIN" "$T/rgitproj" 2> "$T/rgit-tasks.err"
# gh ran here, told the repository by name, for the host's branch.
grep -q '^pr view fix .*--repo me/rgit' "$T/rgit-gh.log" \
    || failed "remote-git: gh was not asked about fix with --repo: $(cat "$T/rgit-gh.log")"
expect "$T/rgit-prs.out" agents_prs "fix=#21:OPEN:running"
# Clean up removed the merged worktree on the host, and its branch.
grep -q '^message: Removed old and its branch' "$T/rgit-cleaned.out" \
    || failed "remote-git: clean up said $(grep '^message:' "$T/rgit-cleaned.out")"
git -C "$RG" worktree list --porcelain | grep -q 'rgit-wt/old$' \
    && failed "remote-git: the merged worktree is still listed: $(git -C "$RG" worktree list)"
[ ! -e "$T/rgit-wt/old" ] || failed "remote-git: the cleaned up worktree's folder is still there"
git -C "$RG" rev-parse -q --verify refs/heads/old >/dev/null \
    && failed "remote-git: the merged branch old is still there"
# The new task's worktree is on the host, beside its main checkout, and its
# terminal is ssh into it.
main_path=$(git -C "$RG" worktree list --porcelain | sed -n '1s/^worktree //p')
task_path="$(dirname "$main_path")/rgit-worktrees/try-it"
git -C "$RG" worktree list --porcelain | grep -qxF "worktree $task_path" \
    || failed "remote-git: no worktree at $task_path: $(git -C "$RG" worktree list)"
[ "$(git -C "$task_path" branch --show-current 2>/dev/null)" = try-it ] \
    || failed "remote-git: the new worktree is not on try-it"
grep -m1 '^agents: ' "$T/rgit-task.out" | grep -q 'selected=try-it ' \
    || failed "remote-git: the new task is not selected: $(grep -m1 '^agents: ' "$T/rgit-task.out")"
grep -qxF -- "sh -c 'cd ${q}$task_path${q} && exec \"\${SHELL:-/bin/sh}\" -l'" "$T/rgit-ssh.log" 2>/dev/null \
    || failed "remote-git: the new task's ssh did not go to $task_path: $(tr '\n' ' ' < "$T/rgit-ssh.log" 2>/dev/null)"
git -C "$RG" worktree remove --force "$task_path" 2>/dev/null || true
