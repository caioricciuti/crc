# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh reorder.

# ---- the terminal's tab strip: reorder, cycle, a long strip ------------------
# Two workspaces whose terminals lie among each other as they were started:
# garden has tab1, tab3 and tab4, sheds has tab2 and tab5 (each shell names
# itself by its number). A tab of the strip is dragged to another place
# among its workspace's tabs, a line showing where it lands; a press that
# does not move is a click, Escape or letting go off the strip leaves the
# tab where it was. Move Tab Left and Right (Cmd-Shift-[ and ], sent here
# as the braces they type) move the terminal showing, Cmd-[ and Cmd-] show
# the next one round the strip, Cmd-1 to 9 count the tabs as the strip has
# them, and a tab's own menu moves it or closes the others. None of it
# touches the other workspace's tabs, and two panes open through a move
# still show the same two sessions. The order is there again after a quit: a
# real one (Cmd-Q), as the script's own quit writes nothing down.
R="$T/garden"
mkdir -p "$R" "$T/sheds" "$T/home/reorder"
printf 'beds\n' > "$R/beds.txt"
cat > "$T/reorder-shell.sh" <<'SHELL'
#!/bin/sh
# A shell that names its tab: the line of $REORDER_TITLES for its number
# in the order the shells were started, else that number.
dir=${REORDER_DIR:-/tmp}
n=$(($(cat "$dir/count" 2>/dev/null || echo 0) + 1))
echo "$n" > "$dir/count"
title=$(sed -n "${n}p" "$dir/titles" 2>/dev/null)
printf '\033]0;%s\007' "${title:-tab$n}"
exec /bin/sh
SHELL
chmod +x "$T/reorder-shell.sh"
mkdir -p "$T/reorder-a"
cat > "$T/reorder.script" <<SCRIPT
wait 500
key 0 cmd,shift A
wait 900
key 45 cmd n
wait 200
text sheds
key 36
wait 900
key 18 ctrl 1
wait 400
key 17 cmd t
wait 700
key 17 cmd t
wait 700
key 19 ctrl 2
wait 400
key 17 cmd t
wait 700
key 18 ctrl 1
wait 500
dump $T/reorder-start.out
downat @terminal.tab.2 0.3 0.5
dragby 2 0
upby 2 0
wait 300
dump $T/reorder-click.out
downat @terminal.tab.3 0.3 0.5
wait 100
dragat @terminal.tab.0 0.1 0.5
wait 200
dump $T/reorder-held.out
upat @terminal.tab.0 0.1 0.5
wait 300
dump $T/reorder-dragged.out
downat @terminal.tab.0 0.3 0.5
dragat @terminal.tab.3 0.9 0.5
wait 100
key 53
wait 100
upat @terminal.tab.3 0.9 0.5
wait 300
dump $T/reorder-escape.out
downat @terminal.tab.0 0.3 0.5
dragat @terminal.tab.3 0.9 0.5
wait 100
upat @terminal 0.5 0.5
wait 300
dump $T/reorder-off.out
key 30 cmd }
wait 300
dump $T/reorder-right.out
key 30 cmd }
key 30 cmd }
key 30 cmd }
wait 300
dump $T/reorder-end.out
key 33 cmd {
wait 300
dump $T/reorder-left.out
key 30 cmd ]
wait 300
dump $T/reorder-next.out
key 30 cmd ]
wait 300
dump $T/reorder-round.out
key 33 cmd [
wait 300
dump $T/reorder-previous.out
key 18 cmd
wait 300
dump $T/reorder-one.out
key 25 cmd
wait 300
dump $T/reorder-nine.out
key 19 cmd
wait 300
key 42 cmd \\
wait 700
dump $T/reorder-pair.out
key 30 cmd }
wait 300
dump $T/reorder-pair-moved.out
key 13 cmd,opt w
wait 300
menuat @terminal.tab.1 0.3 0.5
wait 100
dump $T/reorder-menu-first.out
menuat @terminal.tab.3 0.3 0.5
wait 100
dump $T/reorder-menu.out
action moveTerminalTabLeft:
wait 300
dump $T/reorder-menu-moved.out
key 19 ctrl 2
wait 400
dump $T/reorder-sheds.out
key 18 ctrl 1
wait 400
key 12 cmd q
wait 300
quit
SCRIPT
reorder_run() {
    HOME="$T/home/reorder" REORDER_DIR="$T/reorder-a" CRC_TERMINAL_SHELL="$T/reorder-shell.sh" \
        CRC_SELFTEST="$1" "$BIN" "$R" 2> "$2"
}
reorder_run "$T/reorder.script" "$T/reorder.err"
expect "$T/reorder-start.out" agents "on tasks=garden,sheds selected=garden sessions=garden,sheds,garden,*garden,sheds claude=0 panes=- op=-"
expect "$T/reorder-start.out" terminal_tabs "tab1,(tab2),tab3,*tab4,(tab5) panes=- scrolled=no drag=-"
# A press that barely moves shows the tab, and moves nothing.
expect "$T/reorder-click.out" terminal_tabs "tab1,(tab2),*tab3,tab4,(tab5) panes=- scrolled=no drag=-"
# The third of garden's, held over the first: it is on its way, and shows.
expect "$T/reorder-held.out" terminal_tabs "tab1,(tab2),tab3,*tab4,(tab5) panes=- scrolled=no drag=tab4"
# Let go there, it is the first; sheds' two are where they were.
expect "$T/reorder-dragged.out" terminal_tabs "*tab4,tab1,(tab2),tab3,(tab5) panes=- scrolled=no drag=-"
expect "$T/reorder-dragged.out" agents "on tasks=garden,sheds selected=garden sessions=*garden,garden,sheds,garden,sheds claude=0 panes=- op=-"
# Escape gives a drag up, and so does letting go off the strip.
expect "$T/reorder-escape.out" terminal_tabs "*tab4,tab1,(tab2),tab3,(tab5) panes=- scrolled=no drag=-"
expect "$T/reorder-off.out" terminal_tabs "*tab4,tab1,(tab2),tab3,(tab5) panes=- scrolled=no drag=-"
# Move Tab Right, once and then to the end, where it stays; and back one.
expect "$T/reorder-right.out" terminal_tabs "tab1,*tab4,(tab2),tab3,(tab5) panes=- scrolled=no drag=-"
expect "$T/reorder-end.out" terminal_tabs "tab1,(tab2),tab3,*tab4,(tab5) panes=- scrolled=no drag=-"
expect "$T/reorder-left.out" terminal_tabs "tab1,(tab2),*tab4,tab3,(tab5) panes=- scrolled=no drag=-"
# Show Next Tab goes along garden's and round to its first; Previous back.
expect "$T/reorder-next.out" terminal_tabs "tab1,(tab2),tab4,*tab3,(tab5) panes=- scrolled=no drag=-"
expect "$T/reorder-round.out" terminal_tabs "*tab1,(tab2),tab4,tab3,(tab5) panes=- scrolled=no drag=-"
expect "$T/reorder-previous.out" terminal_tabs "tab1,(tab2),tab4,*tab3,(tab5) panes=- scrolled=no drag=-"
# Cmd-1 and Cmd-9 count the strip as it is now.
expect "$T/reorder-one.out" terminal_tabs "*tab1,(tab2),tab4,tab3,(tab5) panes=- scrolled=no drag=-"
expect "$T/reorder-nine.out" terminal_tabs "tab1,(tab2),tab4,*tab3,(tab5) panes=- scrolled=no drag=-"
# Cmd-2 shows tab4 and Cmd-\ puts tab1 in a pane beside it. Moved a place
# along with the panes open, the same two sessions are the panes, by other
# numbers. Cmd-Option-W then closes tab1's pane, and tab4 has the keyboard.
expect "$T/reorder-pair.out" terminal_tabs "*tab1,(tab2),tab4,tab3,(tab5) panes=h(0.5,tab4,*tab1) scrolled=no drag=-"
expect "$T/reorder-pair.out" agents "on tasks=garden,sheds selected=garden sessions=*garden,sheds,garden,garden,sheds claude=0 panes=h(0.5,2,*0) op=-"
expect "$T/reorder-pair-moved.out" terminal_tabs "(tab2),tab4,*tab1,tab3,(tab5) panes=h(0.5,tab4,*tab1) scrolled=no drag=-"
expect "$T/reorder-pair-moved.out" agents "on tasks=garden,sheds selected=garden sessions=sheds,garden,*garden,garden,sheds claude=0 panes=h(0.5,1,*2) op=-"
# A tab's own menu: the first cannot go left; the last can, and does.
expect "$T/reorder-menu-first.out" message "menu: Close Terminal|Close Other Terminals|Move Tab Right|Split Right|Split Down"
expect "$T/reorder-menu.out" message "menu: Close Terminal|Close Other Terminals|Move Tab Left|Split Right|Split Down"
expect "$T/reorder-menu-moved.out" terminal_tabs "(tab2),*tab4,tab3,tab1,(tab5) panes=- scrolled=no drag=-"
# The other workspace's strip, untouched by all of it.
expect "$T/reorder-sheds.out" terminal_tabs "tab2,(tab4),(tab3),(tab1),*tab5 panes=- scrolled=no drag=-"

# The next window has the tabs in the order they were left in, not the
# order they were started in. A tab's menu is about that tab whatever
# closes while it is open: with garden's tab4, tab3, tab1 and a new tab6,
# the menu is made on tab3, tab4 before it is closed, and the menu's Close
# closes tab3, not the tab that slid into its place. Close Others then
# closes garden's other one and neither of sheds'; a press on a close
# button closes, and is no drag.
cat > "$T/reorder-2.script" <<SCRIPT
wait 900
key 0 cmd,shift A
wait 600
key 18 ctrl 1
wait 500
dump $T/reorder-back.out
key 17 cmd t
wait 700
menuat @terminal.tab.2 0.3 0.5
clickat @terminal.close.1 0.5 0.5
wait 300
action closeTerminalTab:
wait 400
dump $T/reorder-stale.out
menuat @terminal.tab.1 0.3 0.5
action closeOtherTerminalTabs:
wait 500
dump $T/reorder-others.out
key 19 ctrl 2
wait 400
downat @terminal.close.2 0.5 0.5
dragat @terminal.tab.0 0.1 0.5
upat @terminal.tab.0 0.1 0.5
wait 400
dump $T/reorder-closed.out
quit
SCRIPT
reorder_run "$T/reorder-2.script" "$T/reorder-2.err"
reorder_back=$(grep -m1 '^terminal_tabs: ' "$T/reorder-back.out" 2>/dev/null | sed 's/^terminal_tabs: //; s/ panes=.*//' | tr -d '()*' || true)
[ "$reorder_back" = "tab2,tab4,tab3,tab1,tab5" ] \
    || failed "reorder-back.out: the tabs came back as [$reorder_back], expected [tab2,tab4,tab3,tab1,tab5]"
expect "$T/reorder-stale.out" terminal_tabs "(tab2),tab1,(tab5),*tab6 panes=- scrolled=no drag=-"
expect "$T/reorder-others.out" terminal_tabs "(tab2),*tab1,(tab5) panes=- scrolled=no drag=-"
expect "$T/reorder-closed.out" terminal_tabs "*tab2,(tab1) panes=- scrolled=no drag=-"

# Twelve terminals with long titles in one workspace: the tabs shrink to
# their least width and the strip scrolls, the last one (the one showing)
# in view, with the new-terminal and hide buttons still there to press.
# The wheel takes the strip back to its start; Cmd-1 brings the first tab
# into view again; and a tab dragged along the scrolled strip lands where
# it was let go.
mkdir -p "$T/reorder-b" "$T/home/reorder-long"
for n in 1 2 3 4 5 6 7 8 9 10 11 12; do
    echo "bed $n: seed orders and watering notes for spring"
done > "$T/reorder-b/titles"
cat > "$T/reorder-long.script" <<SCRIPT
resize 1100 700
wait 500
key 0 cmd,shift A
wait 900
key 17 cmd t
wait 500
key 17 cmd t
wait 500
key 17 cmd t
wait 500
key 17 cmd t
wait 500
key 17 cmd t
wait 500
key 17 cmd t
wait 500
key 17 cmd t
wait 500
key 17 cmd t
wait 500
key 17 cmd t
wait 500
key 17 cmd t
wait 500
key 17 cmd t
wait 700
dump $T/reorder-long.out
wheelat @terminal.tab.11 0.5 0.5 4000
wait 300
dump $T/reorder-long-wheel.out
downat @terminal.tab.1 0.3 0.5
wait 100
dragat @terminal.tab.0 0.1 0.5
wait 100
upat @terminal.tab.0 0.1 0.5
wait 300
dump $T/reorder-long-dragged.out
key 25 cmd
wait 300
dump $T/reorder-long-last.out
click @terminal.new
wait 700
dump $T/reorder-long-new.out
quit
SCRIPT
HOME="$T/home/reorder-long" REORDER_DIR="$T/reorder-b" CRC_TERMINAL_SHELL="$T/reorder-shell.sh" \
    CRC_SELFTEST="$T/reorder-long.script" "$BIN" "$R" 2> "$T/reorder-long.err"
reorder_tabs() {
    grep -m1 '^terminal_tabs: ' "$1" 2>/dev/null | sed 's/^terminal_tabs: //' || true
}
reorder_long=$(reorder_tabs "$T/reorder-long.out")
case "$reorder_long" in
    "bed 1: "*",*bed 12: "*" panes=- scrolled=yes drag=-") ;;
    *) failed "reorder-long.out: terminal_tabs is [$reorder_long], expected twelve, the last showing, scrolled" ;;
esac
reorder_long=$(reorder_tabs "$T/reorder-long-wheel.out")
case "$reorder_long" in
    "bed 1: "*" panes=- scrolled=no drag=-") ;;
    *) failed "reorder-long-wheel.out: terminal_tabs is [$reorder_long], expected the strip back at its start" ;;
esac
reorder_long=$(reorder_tabs "$T/reorder-long-dragged.out")
case "$reorder_long" in
    "*bed 2: "*",bed 1: "*",bed 3: "*" panes=- scrolled=no drag=-") ;;
    *) failed "reorder-long-dragged.out: terminal_tabs is [$reorder_long], expected the second tab first" ;;
esac
reorder_long=$(reorder_tabs "$T/reorder-long-last.out")
case "$reorder_long" in
    *",*bed 12: "*" panes=- scrolled=yes drag=-") ;;
    *) failed "reorder-long-last.out: terminal_tabs is [$reorder_long], expected the last tab showing, in view" ;;
esac
# The new-terminal button was in reach at the end of the scrolled strip.
reorder_long=$(reorder_tabs "$T/reorder-long-new.out")
case "$reorder_long" in
    *",*tab13 panes=- scrolled=yes drag=-") ;;
    *) failed "reorder-long-new.out: terminal_tabs is [$reorder_long], expected a thirteenth, showing" ;;
esac
