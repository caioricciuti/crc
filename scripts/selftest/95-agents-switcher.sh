# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh agents-switcher.

# ---- Cmd-P in Agents mode switches sessions --------------------------------
# Two workspaces on one folder (swproj and swproj 2), a shell each. Shown
# one then the other, Cmd-P lists the one before first, so Return goes
# back to it. `>` still lists commands, and `/` the project's files.
W="$T/swproj"
mkdir -p "$W" "$T/home/sw-home"
printf 'x\n' > "$W/switchme.txt"
cat > "$T/sw.script" <<SCRIPT
wait 500
key 0 cmd,shift A
wait 900
key 45 cmd n
wait 200
text swproj
key 36
wait 700
key 18 ctrl 1
wait 400
key 19 ctrl 2
wait 400
dump $T/sw-second.out
key 35 cmd p
wait 300
dump $T/sw-open.out
key 36
wait 500
dump $T/sw-back.out
key 35 cmd p
wait 200
text >
wait 300
dump $T/sw-commands.out
key 53
wait 200
key 35 cmd p
wait 200
text /switchme
wait 300
dump $T/sw-files.out
key 53
quit
SCRIPT
HOME="$T/home/sw-home" CRC_TERMINAL_SHELL=/bin/sh \
    CRC_SELFTEST="$T/sw.script" "$BIN" "$W" 2> "$T/sw.err"
expect "$T/sw-second.out" agents "on tasks=swproj,swproj 2 selected=swproj 2 sessions=swproj,*swproj 2 claude=0 pair=- op=-"
# The one shown before the current is first; the current comes second.
expect "$T/sw-open.out" palette_first "swproj / sh"
expect "$T/sw-back.out" agents "on tasks=swproj,swproj 2 selected=swproj sessions=*swproj,swproj 2 claude=0 pair=- op=-"
expect "$T/sw-back.out" palette_query ""
# `>` is the commands, never a session.
expect "$T/sw-commands.out" palette_query ">"
expect "$T/sw-commands.out" palette_first "About crc"
expect "$T/sw-files.out" palette_first "switchme.txt"
