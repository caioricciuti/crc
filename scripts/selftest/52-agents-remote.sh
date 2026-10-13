# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh remote.

# ---- A remote workspace's terminals open over ssh ---------------------------
# A remote workspace is a folder on an ssh host: kept as a fourth field in
# the workspaces file (seeded here), or typed as `host:path` into New
# Workspace. Its terminal is ssh to that folder, here a stand-in named by
# CRC_SSH_COMMAND that writes down its arguments and waits. Nothing of this
# Mac is read for it: no Git, no review of what changes here, and the
# inspector shows where it is instead. A host ssh cannot reach (the
# stand-in fails as ssh does) leaves its terminal open, saying why, and its
# inspector says it is unreachable.
RH="$T/home/remote"
RS="$RH/Library/Application Support/crc"
# The project is under the home folder, which the remote terminal starts
# in here: a file written in it is never that terminal's to review.
RP="$RH/remoteproj"
mkdir -p "$RP" "$RS/agents" "$RH/.ssh"
printf 'one\n' > "$RP/a.txt"
printf 'app\t/srv/app\tws:remote1\tmyhost\n' > "$RS/agents/workspaces"
printf 'Host shots\n  HostName 10.0.0.2\n' > "$RH/.ssh/config"
# Its Git is read over ssh too: those calls (BatchMode) run here, where
# neither folder is, so each finds nothing (scenario 53 has a host's Git).
cat > "$T/remote-ssh.sh" <<STUB
#!/bin/sh
for last; do :; done
case " \$* " in *" nohost "*)
    echo "ssh: Could not resolve hostname nohost: nodename nor servname provided, or not known" >&2
    exit 255
    ;;
esac
case " \$* " in *BatchMode=yes*) exec /bin/sh -c "\$last" ;; esac
printf '%s\n' "\$@" >> "$T/remote-ssh.log"
echo '==' >> "$T/remote-ssh.log"
echo 'remote stub up'
sleep 30
STUB
chmod +x "$T/remote-ssh.sh"
cat > "$T/remote.script" <<SCRIPT
wait 500
key 0 cmd,shift A
wait 900
wait 700
dump $T/remote-on.out
click @agents.task.0
wait 700
wait 700
dump $T/remote-term.out
wait 700
wait 700
wait 700
dump $T/remote-review.out
key 45 cmd,shift N
wait 200
text box:/home/me/work
key 36
wait 700
wait 700
dump $T/remote-new.out
key 45 cmd,shift N
wait 200
text nohost:/srv/x
key 36
wait 700
wait 700
wait 700
dump $T/remote-down.out
quit
SCRIPT
# Written by another process (the watcher does not hear crc's own writes)
# while both terminals count as working, the remote one the latest.
(
    sleep 4
    printf 'written while the remote terminal works\n' > "$RP/notes.txt"
) &
remote_writer=$!
HOME="$RH" CRC_TERMINAL_SHELL=/bin/sh CRC_SSH_COMMAND="$T/remote-ssh.sh" \
    CRC_SELFTEST="$T/remote.script" "$BIN" "$RP" 2> "$T/remote.err"
wait "$remote_writer" 2>/dev/null || true
# The local shell gets a workspace of its own; the remote one takes none.
expect "$T/remote-on.out" agents "on tasks=app,remoteproj selected=remoteproj sessions=*remoteproj claude=0 pair=- op=-"
expect "$T/remote-term.out" agents "on tasks=app,remoteproj selected=app sessions=remoteproj,*app claude=0 pair=- op=-"
expect "$T/remote-term.out" agents_tree "app,remoteproj"
# Read on the host: no repository in its folder there, said as one here is.
expect "$T/remote-term.out" inspector "remote=myhost:/srv/app none"
expect_terminal "$T/remote-term.out" "remote stub up"
# A file written here while the remote terminal prints is not its to
# review: the local one, also working, takes it.
expect "$T/remote-review.out" terminals "idle review=1|idle"
expect "$T/remote-new.out" agents "on tasks=app,remoteproj,work selected=work sessions=remoteproj,app,*work claude=0 pair=- op=-"
expect "$T/remote-new.out" inspector "remote=box:/home/me/work none"
# A host ssh cannot resolve: its terminal stays, with ssh's error and what
# to check, and its folder is unreachable instead of being read forever.
expect "$T/remote-down.out" agents "on tasks=app,remoteproj,work,x selected=x sessions=remoteproj,app,work,*x claude=0 pair=- op=-"
expect_terminal "$T/remote-down.out" "Could not resolve hostname nohost"
expect_terminal "$T/remote-down.out" "crc: could not connect to nohost. Check the host name"
expect_terminal "$T/remote-down.out" "hosts are: shots"
expect_terminal "$T/remote-down.out" "Press Return to close this tab."
expect "$T/remote-down.out" agents_tree "app,remoteproj,work,x(unreachable)"
expect "$T/remote-down.out" inspector "remote=nohost:/srv/x unreachable"
log="$T/remote-ssh.log"
# The line ssh hands the login shell is sh's: `sh -c` and the line quoted.
q="'\"'\"'"
for arg in -t -- myhost "sh -c 'cd ${q}/srv/app${q} && exec \"\${SHELL:-/bin/sh}\" -l'" box "sh -c 'cd ${q}/home/me/work${q} && exec \"\${SHELL:-/bin/sh}\" -l'"; do
    grep -qxF -- "$arg" "$log" 2>/dev/null || failed "remote: ssh was not given [$arg]: $(tr '\n' ' ' < "$log" 2>/dev/null)"
done
# The second line is kept as typed, its host after the key; the seeded one
# is written back as it was.
grep -qxF "$(printf 'app\t/srv/app\tws:remote1\tmyhost')" "$RS/agents/workspaces" \
    || failed "remote: the seeded workspace changed: $(cat "$RS/agents/workspaces")"
grep -q "$(printf '^work\t/home/me/work\tws:[^\t]*\tbox$')" "$RS/agents/workspaces" \
    || failed "remote: the typed workspace was not kept: $(cat "$RS/agents/workspaces")"
[ "$(stat -f %Lp "$RS/ssh" 2>/dev/null)" = 700 ] \
    || failed "remote: the ssh socket folder is not private: $(stat -f %Lp "$RS/ssh" 2>&1)"
