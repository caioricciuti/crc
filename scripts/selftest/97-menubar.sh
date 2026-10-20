# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh menubar.

# ---- The menu bar item: the count waiting, and a row per workspace ---------
# A test instance has no item in the menu bar; the dump says what it would
# show, and `menubar N` makes its menu as it opens and chooses its Nth
# workspace row. Two workspaces on one folder (mbproj and mbproj 2), a
# shell each; in the second a stand-in agent ends its turn with the Stop
# hook, so its session waits. Shown the first, the menu still lists the
# waiting one first, the count 1. Back in the editor, choosing that row
# turns Agents mode on and shows its waiting session.
case "$BIN" in /*) mb_bin=$BIN ;; *) mb_bin=$PWD/$BIN ;; esac
W="$T/mbproj"
mkdir -p "$W" "$T/home/mb-home" "$T/mbbin"
printf 'x\n' > "$W/menubar.txt"
cat > "$T/mbbin/mbstub" <<AGENT
#!/bin/sh
printf 'stub ready\n'
printf '{}' | "$mb_bin" --hook stop
sleep 30
AGENT
chmod +x "$T/mbbin/mbstub"
cat > "$T/mb.script" <<SCRIPT
wait 500
key 0 cmd,shift A
wait 900
key 45 cmd n
wait 200
text mbproj
key 36
wait 700
text mbstub
key 36
wait 700
wait 700
key 18 ctrl 1
wait 3500
dump $T/mb-rows.out
key 0 cmd,shift A
wait 400
dump $T/mb-editor.out
menubar 1
wait 500
dump $T/mb-pick.out
quit
SCRIPT
HOME="$T/home/mb-home" PATH="$T/mbbin:$PATH" CRC_TERMINAL_SHELL=/bin/sh \
    CRC_SELFTEST="$T/mb.script" "$BIN" "$W" 2> "$T/mb.err"
expect "$T/mb-rows.out" agents "on tasks=mbproj,mbproj 2 selected=mbproj sessions=*mbproj,mbproj 2 claude=0 pair=- op=-"
# Waiting first though it is second in the list and not the one showing.
expect "$T/mb-rows.out" menubar "1 mbproj 2 · waiting|mbproj · idle"
expect "$T/mb-editor.out" agents "off tasks=mbproj,mbproj 2 selected=mbproj sessions=*mbproj,mbproj 2 claude=0 pair=- op=-"
# The row chosen: Agents mode, that workspace, its waiting session.
expect "$T/mb-pick.out" agents "on tasks=mbproj,mbproj 2 selected=mbproj 2 sessions=mbproj,*mbproj 2 claude=0 pair=- op=-"
expect "$T/mb-pick.out" menubar "1 mbproj 2 · waiting|mbproj · idle"
if grep -q 'no menu bar row' "$T/mb.err"; then
    failed "menubar: $(cat "$T/mb.err")"
fi
