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
RG="$T/rgit"
RGH="$T/home/rgit"
RGS="$RGH/Library/Application Support/crc"
mkdir -p "$RG" "$T/rgitproj" "$RGS/agents"
printf 'one\n' > "$RG/a.txt"
printf 'one\n' > "$T/rgitproj/a.txt"
testgit "$RG" init -q -b main
testgit "$RG" add a.txt
testgit "$RG" commit -qm one
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
    [ -e "$T/rgit-down" ] && exit 255
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
expect "$T/rgit-term.out" inspector "remote=myhost:$fix_path"
expect_terminal "$T/rgit-term.out" "remote stub up"
grep -qxF -- "cd '$fix_path' && exec \"\${SHELL:-/bin/sh}\" -l" "$T/rgit-ssh.log" 2>/dev/null \
    || failed "remote-git: the worktree's ssh did not go to $fix_path: $(tr '\n' ' ' < "$T/rgit-ssh.log" 2>/dev/null)"
# Every read asked nothing. (The shared connection's socket would not fit
# under this run's long temporary HOME, so it is not asked for here; the
# unit tests check its options.)
blog="$T/rgit-batch.log"
for arg in "BatchMode=yes" "ConnectTimeout=5" "-- myhost cd -P '$RG'" "'worktree' 'list' '--porcelain'" "'rev-list' '--count' 'main..fix'"; do
    grep -qF -- "$arg" "$blog" 2>/dev/null || failed "remote-git: no read over ssh had [$arg]"
done
