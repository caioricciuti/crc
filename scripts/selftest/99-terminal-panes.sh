# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh terminal-panes.

# ---- the terminal's panes: split, focus, drag a divider, close --------------
# Two workspaces: plots has the shells one, two and three, sheds has shed
# (each shell names its tab, and knows its number in the order they were
# started: one 1, shed 2, two 3, three 4). In plots Cmd-\ puts a new shell
# in a pane to the right and Cmd-Shift-\ one under that, three panes. What
# is typed goes to the pane with the keyboard and no other: each shell runs
# `mark`, which writes down its own number, the word it was given and the
# size its terminal says it has. The keyboard goes round the panes, to the
# one beside by direction, and to the one clicked. A divider dragged moves
# the panes with it and tells no program a size until it is let go, then
# each once; it stops where a pane would have fewer than twenty columns,
# and a double click evens it. The wheel scrolls the pane under it and a
# triple click selects in the pane clicked, the others left as they were.
# Close Pane takes the pane with the keyboard and keeps its tab; a tab of
# the strip shows in the pane with the keyboard; the other workspace has
# its own single session, and coming back the panes are as they were left,
# nobody told a size. A tab that is a pane closes: the pane beside it has
# the room. A tab's own menu puts it in a pane of its own.
P="$T/plots"
mkdir -p "$P" "$T/sheds" "$T/home/panes" "$T/panes" "$T/panesbin"
printf 'beans\n' > "$P/beds.txt"
printf 'one\nshed\ntwo\nthree\n' > "$T/panes/titles"
cat > "$T/panes-shell.sh" <<'SHELL'
#!/bin/sh
# A shell that names its tab by the line of the titles file for its number
# in the order the shells were started, and keeps that number.
dir=${PANES_DIR:-/tmp}
n=$(($(cat "$dir/count" 2>/dev/null || echo 0) + 1))
echo "$n" > "$dir/count"
title=$(sed -n "${n}p" "$dir/titles" 2>/dev/null)
printf '\033]0;%s\007' "${title:-shell$n}"
PANE_N=$n
export PANE_N
exec /bin/sh
SHELL
cat > "$T/panesbin/mark" <<'MARK'
#!/bin/sh
# Writes down which shell ran it, the word it was given, and the rows and
# columns its terminal says it has.
echo "$PANE_N $1 $(stty size)" >> "$PANES_DIR/marks"
MARK
chmod +x "$T/panes-shell.sh" "$T/panesbin/mark"
cat > "$T/panes.script" <<SCRIPT
resize 1280 800
wait 500
key 0 cmd,shift A
wait 900
key 45 cmd n
wait 200
text sheds
key 36
wait 900
key 18 ctrl 1
wait 500
dump $T/panes-start.out
key 42 cmd \\
wait 900
dump $T/panes-right.out
key 42 cmd |
wait 900
dump $T/panes-three.out
text mark c
key 36
wait 300
key 33 cmd,opt [
wait 200
dump $T/panes-previous.out
text mark b
key 36
wait 300
action focusPaneLeft:
wait 200
dump $T/panes-left.out
text mark a
key 36
wait 400
action focusPaneLeft:
action focusPaneUp:
wait 200
dump $T/panes-edge.out
action focusPaneRight:
wait 200
dump $T/panes-toright.out
action focusPaneDown:
wait 200
dump $T/panes-down.out
key 30 cmd,opt ]
wait 200
dump $T/panes-round.out
clickat @terminal.pane.2 0.5 0.5
wait 200
dump $T/panes-clicked.out
downat @terminal.split.1 0.5 0.3
dragby -60 0
dragby -150 0
wait 300
dump $T/panes-held.out
upby -150 0
wait 500
dump $T/panes-dragged.out
text mark d
key 36
wait 300
clickat @terminal.pane.1 0.5 0.5
text mark e
key 36
wait 300
clickat @terminal.pane.3 0.5 0.5
text mark f
key 36
wait 400
downat @terminal.split.1 0.5 0.6
dragby -3000 0
wait 300
dump $T/panes-clamp-held.out
upby -3000 0
wait 500
dump $T/panes-clamped.out
clickin @terminal.split.1 8 60 2
wait 500
dump $T/panes-evened.out
downat @terminal.split.2 0.4 0.5
dragby 0 -90
upby 0 -90
wait 500
dump $T/panes-stacked.out
clickin @terminal.split.2 40 8 2
wait 400
clickat @terminal.pane.2 0.5 0.5
text seq 1 300
key 36
wait 600
clickat @terminal.pane.1 0.5 0.5
wait 200
wheelat @terminal.pane.2 0.5 0.5 200
wait 200
clickin @terminal.pane.3 30 8 3
wait 200
dump $T/panes-scrolled.out
clickat @terminal.pane.1 0.5 0.5
wait 200
dump $T/panes-kept.out
action focusPaneRight:
wait 200
key 13 cmd,opt w
wait 500
dump $T/panes-closed.out
clickat @terminal.tab.2 0.3 0.5
wait 500
dump $T/panes-shown.out
key 19 ctrl 2
wait 500
dump $T/panes-sheds.out
key 18 ctrl 1
wait 500
dump $T/panes-back.out
text mark g
key 36
wait 300
clickat @terminal.close.0 0.5 0.5
wait 500
dump $T/panes-tab-closed.out
menuat @terminal.tab.2 0.3 0.5
action splitTerminalTabDown:
wait 500
dump $T/panes-menu.out
key 13 cmd w
wait 500
dump $T/panes-cmdw.out
text mark h
key 36
wait 400
dump $T/panes-end.out
quit
SCRIPT
HOME="$T/home/panes" PATH="$T/panesbin:$PATH" PANES_DIR="$T/panes" CRC_TERMINAL_SHELL="$T/panes-shell.sh" \
    CRC_SELFTEST="$T/panes.script" "$BIN" "$P" 2> "$T/panes.err"

# panes_of <dump> <title> <what>: of the pane showing that session, its
# grid (cols, rows), how often its program was told a size (told), how far
# back it is scrolled (back), whether it has a selection (picked), or its
# place and size on screen (x, y, w, h); empty when it is no pane.
panes_of() {
    grep -m1 '^terminal_panes: ' "$1" 2>/dev/null | tr ' ' '\n' | awk -v pane="$2" -v what="$3" '
        {
            split($0, sides, "@")
            count = split(sides[1], part, ":")
            if (count != 5 || part[1] != pane) next
            split(part[2], grid, "x")
            split(sides[2], box, ",")
            if (what == "cols") print grid[1]
            if (what == "rows") print grid[2]
            if (what == "told") print substr(part[3], 2)
            if (what == "back") print substr(part[4], 2)
            if (what == "picked") print substr(part[5], 2)
            if (what == "x") print box[1]
            if (what == "y") print box[2]
            if (what == "w") print box[3]
            if (what == "h") print box[4]
        }'
}
# panes_same <dump> <dump> <what>: the three panes of plots are the same in
# both dumps in that.
panes_same() {
    local pane
    for pane in one two three; do
        [ -n "$(panes_of "$1" "$pane" "$3")" ] \
            && [ "$(panes_of "$1" "$pane" "$3")" = "$(panes_of "$2" "$pane" "$3")" ] \
            || failed "$(basename "$2"): $pane's $3 is [$(panes_of "$2" "$pane" "$3")], was [$(panes_of "$1" "$pane" "$3")] in $(basename "$1")"
    done
}
# panes_told <before> <after> <title> <more>: that session's program was
# told a size that many times between the two dumps.
panes_told() {
    local was now
    was=$(panes_of "$1" "$3" told)
    now=$(panes_of "$2" "$3" told)
    [ -n "$was" ] && [ -n "$now" ] && [ "$((now - was))" = "$4" ] \
        || failed "$(basename "$2"): $3 was told a size [$now] times, [$was] in $(basename "$1"), expected $4 more"
}
# panes_mark <word> <shell> <dump> <title>: the shell with that number, and
# no other, ran `mark word`, on a terminal the size the dump has for it.
panes_mark() {
    local got want
    got=$(grep " $1 " "$T/panes/marks" 2>/dev/null || true)
    want="$2 $1 $(panes_of "$3" "$4" rows) $(panes_of "$3" "$4" cols)"
    [ "$got" = "$want" ] || failed "panes: mark $1 was written as [$got], expected [$want]"
}

# One session, then a second in a pane to its right with the keyboard, then
# a third under the second.
expect "$T/panes-start.out" agents "on tasks=plots,sheds selected=plots sessions=*plots,sheds claude=0 panes=- op=-"
expect "$T/panes-start.out" terminal_tabs "*one,(shed) panes=- scrolled=no drag=-"
expect "$T/panes-right.out" terminal_tabs "one,(shed),*two panes=h(0.5,one,*two) scrolled=no drag=-"
expect "$T/panes-three.out" terminal_tabs "one,(shed),two,*three panes=h(0.5,one,v(0.5,two,*three)) scrolled=no drag=-"
expect "$T/panes-three.out" agents "on tasks=plots,sheds selected=plots sessions=plots,sheds,plots,*plots claude=0 panes=h(0.5,0,v(0.5,2,*3)) op=-"
# Each split told the pane it cut and the new one their sizes, once, and
# the pane it did not touch nothing.
panes_told "$T/panes-right.out" "$T/panes-three.out" one 0
panes_told "$T/panes-right.out" "$T/panes-three.out" two 1
[ "$(panes_of "$T/panes-three.out" three told)" = 1 ] \
    || failed "panes-three.out: three was told a size $(panes_of "$T/panes-three.out" three told) times, expected once"
[ "$(panes_of "$T/panes-right.out" one told)" = "$(($(panes_of "$T/panes-start.out" one told) + 1))" ] \
    || failed "panes-right.out: one was not told its half once: $(grep '^terminal_panes:' "$T/panes-right.out")"
# The panes tile the screen: one beside the other two, two over three.
[ "$(panes_of "$T/panes-three.out" two x)" = "$(panes_of "$T/panes-three.out" three x)" ] \
    && [ "$(panes_of "$T/panes-three.out" one h)" -gt "$(panes_of "$T/panes-three.out" two h)" ] \
    && [ "$(panes_of "$T/panes-three.out" three y)" -gt "$(panes_of "$T/panes-three.out" two y)" ] \
    || failed "panes-three.out: the panes are not one beside two over three: $(grep '^terminal_panes:' "$T/panes-three.out")"
# Typed with the keyboard in three, then two (Focus Previous Pane), then
# one (Focus Pane Left): each word reached its own shell, whose terminal is
# the size of its pane.
expect "$T/panes-previous.out" terminal_tabs "one,(shed),*two,three panes=h(0.5,one,v(0.5,*two,three)) scrolled=no drag=-"
expect "$T/panes-left.out" terminal_tabs "*one,(shed),two,three panes=h(0.5,*one,v(0.5,two,three)) scrolled=no drag=-"
panes_mark c 4 "$T/panes-three.out" three
panes_mark b 3 "$T/panes-three.out" two
panes_mark a 1 "$T/panes-three.out" one
# Nothing is left of one or above it: the keyboard stays. To the right is
# the top of the column, under it three; Focus Next Pane goes round to one.
expect "$T/panes-edge.out" terminal_tabs "*one,(shed),two,three panes=h(0.5,*one,v(0.5,two,three)) scrolled=no drag=-"
expect "$T/panes-toright.out" terminal_tabs "one,(shed),*two,three panes=h(0.5,one,v(0.5,*two,three)) scrolled=no drag=-"
expect "$T/panes-down.out" terminal_tabs "one,(shed),two,*three panes=h(0.5,one,v(0.5,two,*three)) scrolled=no drag=-"
expect "$T/panes-round.out" terminal_tabs "*one,(shed),two,three panes=h(0.5,*one,v(0.5,two,three)) scrolled=no drag=-"
expect "$T/panes-clicked.out" terminal_tabs "one,(shed),*two,three panes=h(0.5,one,v(0.5,*two,three)) scrolled=no drag=-"
# The divider between one and the rest, held 150 points to the left of
# where it was pressed: the panes are there already, and no program has
# been told.
grep -q '^terminal_panes: .* held=0$' "$T/panes-held.out" \
    || failed "panes-held.out: no divider is held: $(grep '^terminal_panes:' "$T/panes-held.out")"
panes_same "$T/panes-clicked.out" "$T/panes-held.out" told
panes_same "$T/panes-clicked.out" "$T/panes-held.out" cols
[ "$(panes_of "$T/panes-held.out" one w)" = "$(($(panes_of "$T/panes-clicked.out" one w) - 150))" ] \
    && [ "$(panes_of "$T/panes-held.out" two w)" = "$(($(panes_of "$T/panes-clicked.out" two w) + 150))" ] \
    && [ "$(panes_of "$T/panes-held.out" three w)" = "$(($(panes_of "$T/panes-clicked.out" three w) + 150))" ] \
    || failed "panes-held.out: the panes did not follow the divider 150 points: $(grep '^terminal_panes:' "$T/panes-held.out"), from $(grep '^terminal_panes:' "$T/panes-clicked.out")"
# Let go: the panes are where they were held, each program told once, and
# each terminal is the size of its pane, as its own shell reads it.
grep -q '^terminal_panes: .* held=-$' "$T/panes-dragged.out" \
    || failed "panes-dragged.out: a divider is still held: $(grep '^terminal_panes:' "$T/panes-dragged.out")"
panes_same "$T/panes-held.out" "$T/panes-dragged.out" w
for panes_pane in one two three; do
    panes_told "$T/panes-held.out" "$T/panes-dragged.out" "$panes_pane" 1
done
[ "$(panes_of "$T/panes-dragged.out" one cols)" -lt "$(panes_of "$T/panes-clicked.out" one cols)" ] \
    && [ "$(panes_of "$T/panes-dragged.out" two cols)" -gt "$(panes_of "$T/panes-clicked.out" two cols)" ] \
    || failed "panes-dragged.out: the grids did not follow: $(grep '^terminal_panes:' "$T/panes-dragged.out")"
panes_mark d 3 "$T/panes-dragged.out" two
panes_mark e 1 "$T/panes-dragged.out" one
panes_mark f 4 "$T/panes-dragged.out" three
# Dragged far past the left edge it stops with twenty columns for one, and
# let go there one is told that; a double click evens it again.
panes_same "$T/panes-dragged.out" "$T/panes-clamp-held.out" told
[ "$(panes_of "$T/panes-clamped.out" one cols)" = 20 ] \
    || failed "panes-clamped.out: one has $(panes_of "$T/panes-clamped.out" one cols) columns, expected the least, 20"
panes_same "$T/panes-clamp-held.out" "$T/panes-clamped.out" w
for panes_pane in one two three; do
    panes_told "$T/panes-clamp-held.out" "$T/panes-clamped.out" "$panes_pane" 1
    panes_told "$T/panes-clamped.out" "$T/panes-evened.out" "$panes_pane" 1
done
expect "$T/panes-evened.out" terminal_tabs "one,(shed),two,*three panes=h(0.5,one,v(0.5,two,*three)) scrolled=no drag=-"
panes_same "$T/panes-three.out" "$T/panes-evened.out" w
panes_same "$T/panes-three.out" "$T/panes-evened.out" cols
# The divider between two and three, 90 points up: only those two hear.
panes_told "$T/panes-evened.out" "$T/panes-stacked.out" one 0
panes_told "$T/panes-evened.out" "$T/panes-stacked.out" two 1
panes_told "$T/panes-evened.out" "$T/panes-stacked.out" three 1
[ "$(panes_of "$T/panes-stacked.out" two h)" = "$(($(panes_of "$T/panes-evened.out" two h) - 90))" ] \
    && [ "$(panes_of "$T/panes-stacked.out" three h)" = "$(($(panes_of "$T/panes-evened.out" three h) + 90))" ] \
    || failed "panes-stacked.out: two and three did not follow their divider 90 points: $(grep '^terminal_panes:' "$T/panes-stacked.out")"
# Two printed 300 lines. With the keyboard in one, the wheel over two
# scrolls two back, and a triple click in three selects there and takes the
# keyboard: one is neither scrolled nor selected in.
[ "$(panes_of "$T/panes-scrolled.out" two back)" -gt 0 ] \
    && [ "$(panes_of "$T/panes-scrolled.out" one back)" = 0 ] \
    && [ "$(panes_of "$T/panes-scrolled.out" three back)" = 0 ] \
    || failed "panes-scrolled.out: the wheel over two did not scroll two alone: $(grep '^terminal_panes:' "$T/panes-scrolled.out")"
[ "$(panes_of "$T/panes-scrolled.out" three picked)" = 1 ] \
    && [ "$(panes_of "$T/panes-scrolled.out" one picked)" = 0 ] \
    && [ "$(panes_of "$T/panes-scrolled.out" two picked)" = 0 ] \
    || failed "panes-scrolled.out: the triple click in three did not select in three alone: $(grep '^terminal_panes:' "$T/panes-scrolled.out")"
expect "$T/panes-scrolled.out" terminal_tabs "one,(shed),two,*three panes=h(0.5,one,v(0.5,two,*three)) scrolled=no drag=-"
# A click back in one: two is still scrolled and three still selected.
panes_same "$T/panes-scrolled.out" "$T/panes-kept.out" back
[ "$(panes_of "$T/panes-kept.out" three picked)" = 1 ] \
    || failed "panes-kept.out: a click in one cleared three's selection: $(grep '^terminal_panes:' "$T/panes-kept.out")"
# Close Pane with the keyboard in two, the middle one: three has its room
# and the keyboard, and two is still a tab.
expect "$T/panes-closed.out" terminal_tabs "one,(shed),two,*three panes=h(0.5,one,*three) scrolled=no drag=-"
[ "$(panes_of "$T/panes-closed.out" three h)" = "$(panes_of "$T/panes-closed.out" one h)" ] \
    || failed "panes-closed.out: three did not get two's room: $(grep '^terminal_panes:' "$T/panes-closed.out")"
# Two's tab clicked: it shows in the pane with the keyboard, in place of
# three, which goes back to the strip.
expect "$T/panes-shown.out" terminal_tabs "one,(shed),*two,three panes=h(0.5,one,*two) scrolled=no drag=-"
# Sheds has its one session; back in plots the panes are as they were
# left, the keyboard in two, and no program was told a size on the way.
expect "$T/panes-sheds.out" agents "on tasks=plots,sheds selected=sheds sessions=plots,*sheds,plots,plots claude=0 panes=- op=-"
expect "$T/panes-sheds.out" terminal_tabs "(one),*shed,(two),(three) panes=- scrolled=no drag=-"
expect "$T/panes-back.out" terminal_tabs "one,(shed),*two,three panes=h(0.5,one,*two) scrolled=no drag=-"
for panes_pane in one two; do
    panes_told "$T/panes-shown.out" "$T/panes-back.out" "$panes_pane" 0
done
panes_mark g 3 "$T/panes-back.out" two
# One's tab closed while it is a pane: two has the room, and the keyboard
# it had.
expect "$T/panes-tab-closed.out" terminal_tabs "(shed),*two,three panes=- scrolled=no drag=-"
# Three's own menu, Split Down: three in a pane under two, with the
# keyboard. Cmd-W closes it there, and two is alone with the keyboard.
expect "$T/panes-menu.out" terminal_tabs "(shed),two,*three panes=v(0.5,two,*three) scrolled=no drag=-"
expect "$T/panes-cmdw.out" terminal_tabs "(shed),*two panes=- scrolled=no drag=-"
panes_mark h 3 "$T/panes-end.out" two
# Every word typed, and nothing else, each from the shell it was typed to.
panes_words=$(cut -d' ' -f1,2 "$T/panes/marks" 2>/dev/null | tr '\n' ',' || true)
[ "$panes_words" = "4 c,3 b,1 a,3 d,1 e,4 f,3 g,3 h," ] \
    || failed "panes: the marks are [$panes_words], expected [4 c,3 b,1 a,3 d,1 e,4 f,3 g,3 h,]"
