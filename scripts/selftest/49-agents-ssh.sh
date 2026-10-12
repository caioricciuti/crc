# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh ssh.

# ---- A terminal inside ssh says so -------------------------------------
# A stand-in `ssh` in the folder (sleep under that name, so the kernel
# reports the program as ssh; on PATH a login shell's path_helper would
# put the real one first): typed into the shell, the tab is titled after its
# destination and the inspector shows the session instead of Git. When it
# ends, both come back.
mkdir -p "$T/sshproj" "$T/home/ssh"
ln -sf /bin/sleep "$T/sshproj/ssh"
cat > "$T/ssh.script" <<SCRIPT
wait 500
key 0 cmd,shift A
wait 900
text ./ssh 6
key 36
wait 700
wait 700
wait 700
wait 300
dump $T/ssh-in.out
frame $T/ssh-in.bmp
wait 2000
wait 2000
wait 1000
key 36
wait 700
wait 300
dump $T/ssh-out.out
quit
SCRIPT
HOME="$T/home/ssh" CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/ssh.script" \
    "$BIN" "$T/sshproj" 2> "$T/ssh.err"
# (The tab's title is drawn from the session, not the dump's raw title;
# ssh-in.bmp shows it.)
expect "$T/ssh-in.out" inspector "ssh=6"
case "$(grep '^inspector:' "$T/ssh-out.out")" in
"inspector: ssh="*) failed "ssh: the inspector still shows the session after it ended" ;;
esac
