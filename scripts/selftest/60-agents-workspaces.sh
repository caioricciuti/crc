# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh agents-workspaces.

# ---- Workspaces: one folder holds any number, and one needs none ----------
# A plain folder (deskproj) gets its workspace and a shell; Cmd-N with the
# folder's own name makes a second workspace on that same folder (a test
# instance takes the folder named as it beside the selected one's), with a
# shell of its own. Each is selected apart, keeps its terminals, and a
# rename in its row keeps them apart. Then each gets a second terminal, so
# their tabs interleave (deskproj, notes, deskproj, notes): a terminal that
# exits, in the background or at the front, or one closed in a pair, gives
# way to its own workspace's next, never another's; when a workspace's last
# one exits, that workspace stays selected, empty: nothing typed or Cmd-W
# reaches the other workspace's terminal behind it. Last, a workspace from a
# name alone: no folder, its shell and Claude in the home folder.
W="$T/deskproj"
mkdir -p "$W" "$T/home/ws-home"
printf 'x\n' > "$W/x.txt"
cat > "$T/ws-agent.sh" <<'AGENT'
#!/bin/sh
echo "claude in $(basename "$PWD")"
sleep 30
AGENT
chmod +x "$T/ws-agent.sh"
cat > "$T/ws.script" <<SCRIPT
wait 500
key 0 cmd,shift A
wait 900
key 45 cmd n
wait 200
text deskproj
key 36
wait 700
dump $T/ws-two.out
key 18 ctrl 1
wait 400
dump $T/ws-first.out
key 19 ctrl 2
wait 400
dump $T/ws-second.out
click @agents.task.1 2
wait 300
text notes
key 36
wait 500
dump $T/ws-renamed.out
key 18 ctrl 1
wait 300
key 17 cmd t
wait 700
key 19 ctrl 2
wait 300
key 17 cmd t
wait 700
dump $T/ws-four.out
key 18 cmd
wait 300
text sleep 2; exit
key 36
key 18 ctrl 1
wait 300
dump $T/ws-before-exit.out
wait 1500
wait 1500
dump $T/ws-background.out
text exit
key 36
wait 700
wait 300
dump $T/ws-front.out
text exit
key 36
wait 700
wait 300
dump $T/ws-empty.out
frame $T/ws-empty.bmp
key 13 cmd w
wait 300
text echo leaked-from-deskproj
key 36
wait 500
dump $T/ws-guarded.out
key 19 ctrl 2
wait 400
dump $T/ws-notes.out
key 42 cmd \\
wait 700
wait 300
dump $T/ws-pair.out
text exit
key 36
wait 700
wait 300
dump $T/ws-pair-exit.out
key 45 cmd n
wait 200
text scratch
key 125
key 125
key 125
key 125
key 36
wait 700
wait 300
text pwd > $T/ws-pwd.txt
key 36
wait 500
dump $T/ws-bare.out
frame $T/ws-bare.bmp
key 8 cmd,shift c
wait 900
dump $T/ws-bare-claude.out
quit
SCRIPT
HOME="$T/home/ws-home" CRC_TERMINAL_SHELL=/bin/sh CRC_CLAUDE_COMMAND="$T/ws-agent.sh" \
    CRC_SELFTEST="$T/ws.script" "$BIN" "$W" 2> "$T/ws.err"
[ -z "${SELFTEST_FRAMES:-}" ] || cp -f "$T/ws-empty.bmp" "$T/ws-bare.bmp" "$SELFTEST_FRAMES/"
ws_kept="$T/home/ws-home/Library/Application Support/crc/agents/workspaces"
# Two workspaces on deskproj, each with its own shell and row.
expect "$T/ws-two.out" agents "on tasks=deskproj,deskproj 2 selected=deskproj 2 sessions=deskproj,*deskproj 2 claude=0 pair=- op=-"
expect "$T/ws-two.out" agents_folders "deskproj=$W,deskproj 2=$W"
expect "$T/ws-first.out" agents "on tasks=deskproj,deskproj 2 selected=deskproj sessions=*deskproj,deskproj 2 claude=0 pair=- op=-"
expect "$T/ws-second.out" agents "on tasks=deskproj,deskproj 2 selected=deskproj 2 sessions=deskproj,*deskproj 2 claude=0 pair=- op=-"
# Renamed, it keeps its terminal and its own key in the file.
expect "$T/ws-renamed.out" agents "on tasks=deskproj,notes selected=notes sessions=deskproj,*notes claude=0 pair=- op=-"
[ "$(awk -F'\t' '$2 ~ /\/deskproj$/ { print $3 }' "$ws_kept" 2>/dev/null | sort -u | wc -l | tr -d ' ')" = 2 ] \
    || failed "workspaces: two on one folder were not kept apart: $(cat "$ws_kept" 2>/dev/null)"
# Interleaved: deskproj's at 0 and 2, notes' at 1 and 3.
expect "$T/ws-four.out" agents "on tasks=deskproj,notes selected=notes sessions=deskproj,notes,deskproj,*notes claude=0 pair=- op=-"
expect "$T/ws-before-exit.out" agents "on tasks=deskproj,notes selected=deskproj sessions=deskproj,notes,*deskproj,notes claude=0 pair=- op=-"
# notes' first exits behind deskproj's: deskproj's stays at the front.
expect "$T/ws-background.out" agents "on tasks=deskproj,notes selected=deskproj sessions=deskproj,*deskproj,notes claude=0 pair=- op=-"
# deskproj's at the front exits: its other one, before it, not notes'.
expect "$T/ws-front.out" agents "on tasks=deskproj,notes selected=deskproj sessions=*deskproj,notes claude=0 pair=- op=-"
# Its last exits: deskproj stays selected, empty; notes' is not taken.
expect "$T/ws-empty.out" agents "on tasks=deskproj,notes selected=deskproj sessions=*notes claude=0 pair=- op=-"
# Cmd-W and typing there reach nothing.
expect "$T/ws-guarded.out" agents "on tasks=deskproj,notes selected=deskproj sessions=*notes claude=0 pair=- op=-"
expect "$T/ws-notes.out" agents "on tasks=deskproj,notes selected=notes sessions=*notes claude=0 pair=- op=-"
if grep -q 'leaked-from-deskproj' "$T/ws-notes.out"; then
    failed "workspaces: typing in an empty workspace reached another's terminal"
fi
# A pair in notes; the pane at the front exits: notes' other shows.
expect "$T/ws-pair.out" agents "on tasks=deskproj,notes selected=notes sessions=notes,*notes claude=0 pair=0+1 op=-"
expect "$T/ws-pair-exit.out" agents "on tasks=deskproj,notes selected=notes sessions=*notes claude=0 pair=- op=-"
# A workspace with no folder: no folder line, no repositories, and its
# shell and Claude start in the home folder.
expect "$T/ws-bare.out" agents "on tasks=deskproj,notes,scratch selected=scratch sessions=notes,*scratch claude=0 pair=- op=-"
expect "$T/ws-bare.out" agents_folders "deskproj=$W,notes=$W,scratch=-"
expect "$T/ws-bare.out" inspector "none"
case "$(cat "$T/ws-pwd.txt" 2>/dev/null)" in
*/home/ws-home) ;;
*) failed "workspaces: the shell without a folder started in [$(cat "$T/ws-pwd.txt" 2>/dev/null)]" ;;
esac
expect "$T/ws-bare-claude.out" agents "on tasks=deskproj,notes,scratch selected=scratch sessions=notes,scratch,*scratch claude=1 pair=- op=-"
expect_terminal "$T/ws-bare-claude.out" "claude in ws-home"
grep -q "^scratch		ws:" "$ws_kept" 2>/dev/null \
    || failed "workspaces: the one without a folder was not kept: $(cat "$ws_kept" 2>/dev/null)"
