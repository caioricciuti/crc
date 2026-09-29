# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh split-panes.

# ---- split panes -----------------------------------------------------------
# Cmd-\ opens an empty pane to the right and focuses it; Cmd-Option-[ and ]
# move focus; a file open in another pane comes to the front there instead
# of opening twice; Cmd-Option-W closes the pane.
mkdir -p "$T/paneproj"
printf 'left\n' > "$T/paneproj/left.txt"
printf 'right\n' > "$T/paneproj/right.txt"
cat > "$T/pane.script" <<SCRIPT
wait 600
wait 100
key 42 cmd \\
wait 100
dump $T/pane-split.out
key 33 cmd,opt [
wait 100
dump $T/pane-left.out
key 30 cmd,opt ]
key 35 cmd p
text right
wait 300
key 36
wait 300
dump $T/pane-right.out
key 33 cmd,opt [
key 35 cmd p
text right
wait 300
key 36
wait 300
dump $T/pane-dedup.out
key 13 cmd,opt w
wait 100
dump $T/pane-closed.out
quit
SCRIPT
CRC_SELFTEST="$T/pane.script" "$BIN" "$T/paneproj/left.txt" 2> "$T/pane.err"
expect "$T/pane-split.out" panes 2
expect "$T/pane-split.out" focused_pane 1
expect "$T/pane-split.out" tabs "Home"
expect "$T/pane-left.out" focused_pane 0
expect "$T/pane-left.out" tabs "left.txt"
expect "$T/pane-right.out" focused_pane 1
expect "$T/pane-right.out" tabs "right.txt"
expect "$T/pane-dedup.out" focused_pane 1
expect "$T/pane-dedup.out" tabs "right.txt"
expect "$T/pane-closed.out" panes 1
expect "$T/pane-closed.out" tabs "left.txt"
