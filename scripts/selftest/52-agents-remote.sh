# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh remote.

# ---- A remote workspace's terminals open over ssh ---------------------------
# A remote workspace is a folder on an ssh host: kept as a fourth field in
# the workspaces file (seeded here), or typed as `host:path` into New
# Workspace. Its terminal is ssh to that folder, here a stand-in named by
# CRC_SSH_COMMAND that writes down its arguments and waits. Nothing of this
# Mac is read for it: no Git, and the inspector shows where it is instead.
RH="$T/home/remote"
RS="$RH/Library/Application Support/crc"
mkdir -p "$T/remoteproj" "$RS/agents"
printf 'one\n' > "$T/remoteproj/a.txt"
printf 'app\t/srv/app\tws:remote1\tmyhost\n' > "$RS/agents/workspaces"
cat > "$T/remote-ssh.sh" <<STUB
#!/bin/sh
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
key 45 cmd,shift N
wait 200
text box:/home/me/work
key 36
wait 700
wait 700
dump $T/remote-new.out
quit
SCRIPT
HOME="$RH" CRC_TERMINAL_SHELL=/bin/sh CRC_SSH_COMMAND="$T/remote-ssh.sh" \
    CRC_SELFTEST="$T/remote.script" "$BIN" "$T/remoteproj" 2> "$T/remote.err"
# The local shell gets a workspace of its own; the remote one takes none.
expect "$T/remote-on.out" agents "on tasks=app,remoteproj selected=remoteproj sessions=*remoteproj claude=0 pair=- op=-"
expect "$T/remote-term.out" agents "on tasks=app,remoteproj selected=app sessions=remoteproj,*app claude=0 pair=- op=-"
expect "$T/remote-term.out" agents_tree "app,remoteproj"
expect "$T/remote-term.out" inspector "remote=myhost:/srv/app"
expect_terminal "$T/remote-term.out" "remote stub up"
expect "$T/remote-new.out" agents "on tasks=app,remoteproj,work selected=work sessions=remoteproj,app,*work claude=0 pair=- op=-"
expect "$T/remote-new.out" inspector "remote=box:/home/me/work"
log="$T/remote-ssh.log"
for arg in -t -- myhost "cd '/srv/app' && exec \"\${SHELL:-/bin/sh}\" -l" box "cd '/home/me/work' && exec \"\${SHELL:-/bin/sh}\" -l"; do
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
