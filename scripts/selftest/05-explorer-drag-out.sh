# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh explorer-drag-out.

# ---- dragging out of the Explorer -------------------------------------------
# Rows dragged from the tree can leave it. Over the terminal the pane under
# the pointer is where their paths are typed when they are let go, each
# quoted for the shell with a space after it, and nothing moves on disk;
# over the editor the files among them open and a folder is passed over.
# Out of the window the drag is handed to the system, which another app
# may only copy from. A test instance begins no system drag: it puts the
# items on a
# pasteboard of its own, where the scripted drag (`dragover`, `dropat`,
# `dragout`) finds them, and says what it handed over (`tree_drag_out:`:
# what another app may do with it, what crc may, and the items). Brought
# back over the window the drag is ours again (`tree_drag:` says
# `returned:`): on the tree it moves, or copies with Option, and on the
# terminal and the editor it is typed and opened as one that never left.
# Escape calls it off wherever it is.
out_project() {
    mkdir -p "$1/recipes" "$1/notes"
    printf 'flour, water, salt\n' > "$1/recipes/bread.md"
    printf 'leeks and potatoes\n' > "$1/recipes/soup.md"
    printf 'primroses by the gate\n' > "$1/notes/a walk in spring.md"
    printf 'milk and eggs\n' > "$1/anna's list.txt"
    printf 'tomatoes in may\n' > "$1/garden.txt"
    printf 'a quiet day\n' > "$1/journal.md"
}
# out_holds <file> <text>: the file is there with this one line in it.
out_holds() {
    if [ "$(cat "$1" 2>/dev/null)" != "$2" ]; then
        failed "drag-out: $1 holds [$(cat "$1" 2>&1)], expected [$2]"
    fi
}
# out_whole <project>: every file is where the fixture put it.
out_whole() {
    out_holds "$1/recipes/bread.md" "flour, water, salt"
    out_holds "$1/recipes/soup.md" "leeks and potatoes"
    out_holds "$1/notes/a walk in spring.md" "primroses by the gate"
    out_holds "$1/anna's list.txt" "milk and eggs"
    out_holds "$1/garden.txt" "tomatoes in may"
    out_holds "$1/journal.md" "a quiet day"
    [ "$(find "$1" -type f | wc -l | tr -d ' ')" = 6 ] || failed "drag-out: $1 has other files than the six it started with: [$(find "$1" -type f | tr '\n' ' ')]"
}
# out_quiet <stderr file>: takes out the one line that is the system's and
# not crc's. Its pasteboard says it whenever a process that is not
# sandboxed reads a file's URL from it, and the read succeeds all the same.
out_quiet() {
    grep -v '^sandbox_extension_consume failed: 1 (Operation not permitted)$' "$1" > "$1.kept" || true
    mv -f "$1.kept" "$1"
}
# out_screen <dump>: what the terminal shows, as one line with nothing
# between its characters: a long path wraps wherever the pane is wide
# enough to, and the dump has a `\n` there and no space the row ended on.
out_screen() {
    grep -m1 '^terminal: ' "$1" 2>/dev/null | sed -e 's/\\n//g' -e 's/ //g' || true
}
# out_typed <dump> <text>: the terminal shows this, spaces aside.
out_typed() {
    local out_want
    out_want=$(printf '%s' "$2" | sed -e 's/ //g')
    case "$(out_screen "$1")" in
        *"$out_want"*) ;;
        *) failed "$(basename "$1"): the terminal does not show [$2]: $(grep -m1 '^terminal: ' "$1")" ;;
    esac
}
# out_untyped <dump> <text>: the terminal shows no such thing.
out_untyped() {
    local out_want
    out_want=$(printf '%s' "$2" | sed -e 's/ //g')
    case "$(out_screen "$1")" in
        *"$out_want"*) failed "$(basename "$1"): the terminal shows [$2], and should not" ;;
        *) ;;
    esac
}
# `show` says each word the shell gave it, one to a line, on the terminal
# and in a file: what the shell made of the paths typed.
mkdir -p "$T/outbin" "$T/outshown" "$T/outshown2" "$T/outshown3"
cat > "$T/outbin/show" <<'SHOW'
#!/bin/sh
for shown in "$@"; do echo "<$shown>"; done | tee -a "$SHOWN_DIR/shown"
SHOW
chmod +x "$T/outbin/show"
# A home to each launch: a terminal is held by a helper that outlives the
# window, and the next launch with the same home would take it back, with
# what the last one typed still on its screen.
mkdir -p "$T/home/out1" "$T/home/out2" "$T/home/out3" "$T/home/out4"

# Onto the terminal and the editor. `show ''` is typed first, with no
# space after it for a script to end a line on: the empty word joins the
# first path, as a space before it would stand between them. One file
# dragged over the terminal lights its pane and, let go, is typed there:
# the terminal has the keyboard, no tab opens, nothing moves. A second
# file let go after it is typed after the space the first one left: two
# words, not one. Return, and the shell has each path as a word of its
# own. Then three at once, a name with spaces, a folder and a name with
# a quote in it: three words. The same
# three let go on the editor open the two files, in the tree's order, and
# the keyboard is in the text of the last; the folder is passed over. A
# folder alone has nothing to do there.
P="$T/outproj"
out_project "$P"
cat > "$T/out.script" <<SCRIPT
resize 1280 800
wait 500
click @toolbar.terminal
wait 700
wait 700
text show ''
downat @sidebar.row.3 0.5 0.5
dragat @sidebar.row.2 0.5 0.5
dump $T/out-start.out
dragat @terminal.pane.1 0.5 0.5
dump $T/out-term-held.out
upat @terminal.pane.1 0.5 0.5
wait 300
dump $T/out-term-typed.out
downat @sidebar.row.4 0.5 0.5
dragat @sidebar.row.2 0.5 0.5
dragat @terminal.pane.1 0.5 0.5
upat @terminal.pane.1 0.5 0.5
wait 300
dump $T/out-term-two.out
key 36
wait 500
dump $T/out-term-one.out
text show ''
clickat @sidebar.row.0 0.5 0.5
wait 300
clickat @sidebar.row.0 0.5 0.5 cmd
clickat @sidebar.row.1 0.5 0.5 cmd
clickat @sidebar.row.2 0.5 0.5 cmd
clickat @sidebar.row.3 0.5 0.5 cmd
dump $T/out-three-picked.out
downat @sidebar.row.2 0.5 0.5
dragat @sidebar.row.4 0.5 0.5
dragat @terminal.pane.1 0.5 0.5
dump $T/out-term-three-held.out
upat @terminal.pane.1 0.5 0.5
wait 300
dump $T/out-term-three-typed.out
key 36
wait 500
dump $T/out-term-three.out
downat @sidebar.row.2 0.5 0.5
dragat @sidebar.row.4 0.5 0.5
dragat @text 0.5 0.5
dump $T/out-editor-held.out
upat @text 0.5 0.5
wait 300
dump $T/out-editor.out
text x
dump $T/out-editor-keys.out
downat @sidebar.row.0 0.5 0.5
dragat @sidebar.row.4 0.5 0.5
dragat @text 0.5 0.5
dump $T/out-editor-folder-held.out
upat @text 0.5 0.5
wait 300
dump $T/out-editor-folder.out
quit
SCRIPT
HOME="$T/home/out1" SHOWN_DIR="$T/outshown" PATH="$T/outbin:$PATH" CRC_TERMINAL_SHELL=/bin/sh \
    CRC_SELFTEST_TRASH="$T/outtrash" CRC_SELFTEST="$T/out.script" "$BIN" "$P" 2> "$T/out.err"
out_quiet "$T/out.err"
# In the tree it is the tree's drag, as before.
expect "$T/out-start.out" tree_drag "garden.txt onto nowhere"
expect "$T/out-start.out" tree_drag_out "none"
expect "$T/out-term-held.out" tree_drag "garden.txt onto terminal"
expect "$T/out-term-held.out" file_ops ""
out_untyped "$T/out-term-held.out" "garden.txt"
expect "$T/out-term-typed.out" tree_drag "none"
expect "$T/out-term-typed.out" tree_keys false
expect "$T/out-term-typed.out" tabs "Home"
expect "$T/out-term-typed.out" file_ops ""
expect_terminal "$T/out-term-typed.out" "open=true focus=true "
out_typed "$T/out-term-typed.out" "show '''$P/garden.txt'"
# Typed, and not run: the shell has said nothing yet.
out_untyped "$T/out-term-typed.out" "<$P/garden.txt>"
expect "$T/out-term-two.out" tree_drag "none"
expect_terminal "$T/out-term-two.out" "open=true focus=true "
out_typed "$T/out-term-two.out" "show '''$P/garden.txt' '$P/journal.md'"
out_typed "$T/out-term-one.out" "<$P/garden.txt>"
out_typed "$T/out-term-one.out" "<$P/journal.md>"
expect "$T/out-three-picked.out" tree_acts_on "notes/a walk in spring.md|recipes|anna's list.txt"
expect "$T/out-term-three-held.out" tree_drag "3 items onto terminal"
expect "$T/out-term-three-typed.out" tree_drag "none"
expect_terminal "$T/out-term-three-typed.out" "open=true focus=true "
# As the dump writes it, a backslash is two.
out_typed "$T/out-term-three-typed.out" "show '''$P/notes/a walk in spring.md' '$P/recipes' '$P/anna'\\\\''s list.txt'"
out_typed "$T/out-term-three.out" "<$P/notes/a walk in spring.md>"
out_typed "$T/out-term-three.out" "<$P/anna's list.txt>"
expect "$T/out-term-three.out" tabs "Home"
expect "$T/out-term-three.out" file_ops ""
# What the shell was given, word for word: five paths, five words, the
# two that were let go one after the other among them.
out_words="<$P/garden.txt>
<$P/journal.md>
<$P/notes/a walk in spring.md>
<$P/recipes>
<$P/anna's list.txt>"
if [ "$(cat "$T/outshown/shown" 2>/dev/null)" != "$out_words" ]; then
    failed "drag-out: the shell was given [$(cat "$T/outshown/shown" 2>&1)], expected [$out_words]"
fi
expect "$T/out-editor-held.out" tree_drag "3 items onto editor"
expect "$T/out-editor-held.out" tabs "Home"
expect "$T/out-editor.out" tree_drag "none"
expect "$T/out-editor.out" tabs "a walk in spring.md | anna's list.txt"
expect "$T/out-editor.out" tree_keys false
expect "$T/out-editor.out" file_ops ""
expect_line "$T/out-editor.out" 1 "milk and eggs"
expect_terminal "$T/out-editor.out" "open=true focus=false "
# The keyboard is in the text, not in the terminal behind it.
expect_line "$T/out-editor-keys.out" 1 "xmilk and eggs"
expect "$T/out-editor-folder-held.out" tree_drag "notes onto nowhere"
expect "$T/out-editor-folder.out" tree_drag "none"
expect "$T/out-editor-folder.out" tabs "a walk in spring.md | anna's list.txt"
expect "$T/out-editor-folder.out" file_ops ""
out_whole "$P"
[ ! -e "$T/outtrash" ] || failed "drag-out: a drag onto the terminal or the editor sent something to the Trash"

# Escape, and out of the window. Escape over the tree, over the terminal
# and over the editor calls the drag off: letting go after it types
# nothing and opens nothing. A drag whose pointer leaves the window is
# handed over: nothing is held in the window any more, the dump says what
# another app may do with it (copy, and nothing else) and what crc may,
# and nothing has moved. Escape ends that too, and nothing is left for a
# drag to bring back. Three rows go out as three items. Back over the
# tree they are ours: a move onto a folder, a copy with Option, nothing
# onto the folder they are in, and nothing held once they leave again;
# over the terminal and the editor a copy, with Escape there ending it.
P2="$T/outproj2"
out_project "$P2"
cat > "$T/out-leave.script" <<SCRIPT
resize 1280 800
wait 500
click @toolbar.terminal
wait 700
wait 700
text show ''
downat @sidebar.row.3 0.5 0.5
dragat @sidebar.row.1 0.5 0.5
dump $T/out-esc-tree-held.out
key 53
dump $T/out-esc-tree.out
upat @sidebar.row.1 0.5 0.5
wait 200
downat @sidebar.row.3 0.5 0.5
dragat @sidebar.row.2 0.5 0.5
dragat @terminal.pane.1 0.5 0.5
dump $T/out-esc-term-held.out
key 53
dump $T/out-esc-term.out
upat @terminal.pane.1 0.5 0.5
wait 200
dump $T/out-esc-term-up.out
downat @sidebar.row.3 0.5 0.5
dragat @sidebar.row.2 0.5 0.5
dragat @text 0.5 0.5
dump $T/out-esc-editor-held.out
key 53
upat @text 0.5 0.5
wait 200
dump $T/out-esc-editor.out
downat @sidebar.row.3 0.5 0.5
dragat @sidebar.row.2 0.5 0.5
drag -40 300
dump $T/out-left.out
up -40 300
dump $T/out-left-up.out
key 53
dump $T/out-left-esc.out
dragover @sidebar.row.1 0.5 0.5
dump $T/out-left-gone.out
dragout
clickat @sidebar.row.2 0.5 0.5 cmd
clickat @sidebar.row.4 0.5 0.5 cmd
downat @sidebar.row.3 0.5 0.5
dragat @sidebar.row.1 0.5 0.5
drag 1400 300
dump $T/out-three-left.out
dragover @sidebar.row.1 0.5 0.5
dump $T/out-back-folder.out
flags opt
dragover @sidebar.row.1 0.5 0.5
dump $T/out-back-copy.out
flags -
dragover @sidebar 0.5 0.9
dump $T/out-back-root.out
dragout
dump $T/out-back-left.out
dragover @terminal.pane.1 0.5 0.5
dump $T/out-back-term.out
dragover @text 0.5 0.5
dump $T/out-back-editor.out
key 53
dump $T/out-back-esc.out
quit
SCRIPT
HOME="$T/home/out2" SHOWN_DIR="$T/outshown2" PATH="$T/outbin:$PATH" CRC_TERMINAL_SHELL=/bin/sh \
    CRC_SELFTEST_TRASH="$T/outtrash2" CRC_SELFTEST="$T/out-leave.script" "$BIN" "$P2" 2> "$T/out-leave.err"
out_quiet "$T/out-leave.err"
expect "$T/out-esc-tree-held.out" tree_drag "garden.txt onto recipes"
expect "$T/out-esc-tree.out" tree_drag "none"
expect "$T/out-esc-term-held.out" tree_drag "garden.txt onto terminal"
expect "$T/out-esc-term.out" tree_drag "none"
expect "$T/out-esc-term-up.out" tree_drag "none"
out_untyped "$T/out-esc-term-up.out" "garden.txt"
expect "$T/out-esc-editor-held.out" tree_drag "garden.txt onto editor"
expect "$T/out-esc-editor.out" tree_drag "none"
expect "$T/out-esc-editor.out" tabs "Home"
expect "$T/out-esc-editor.out" file_ops ""
expect "$T/out-left.out" tree_drag "none"
expect "$T/out-left.out" tree_drag_out "outside=copy inside=copy|move items=garden.txt"
expect "$T/out-left.out" file_ops ""
expect "$T/out-left.out" tabs "Home"
expect "$T/out-left-up.out" tree_drag "none"
expect "$T/out-left-up.out" tree_drag_out "outside=copy inside=copy|move items=garden.txt"
expect "$T/out-left-esc.out" tree_drag_out "none"
expect "$T/out-left-esc.out" tree_drag "none"
expect "$T/out-left-esc.out" file_ops ""
expect "$T/out-left-gone.out" tree_drag "none"
expect "$T/out-left-gone.out" file_drag "none"
expect "$T/out-three-left.out" tree_drag "none"
expect "$T/out-three-left.out" tree_drag_out "outside=copy inside=copy|move items=anna's list.txt|garden.txt|journal.md"
expect "$T/out-back-folder.out" tree_drag "returned: 3 items onto recipes"
expect "$T/out-back-folder.out" file_drag "move"
expect "$T/out-back-copy.out" tree_drag "returned: 3 items copy onto recipes"
expect "$T/out-back-copy.out" file_drag "copy"
expect "$T/out-back-root.out" tree_drag "returned: 3 items onto nowhere"
expect "$T/out-back-root.out" file_drag "none"
expect "$T/out-back-left.out" tree_drag "none"
expect "$T/out-back-left.out" file_drag "left"
expect "$T/out-back-left.out" tree_drag_out "outside=copy inside=copy|move items=anna's list.txt|garden.txt|journal.md"
expect "$T/out-back-term.out" tree_drag "returned: 3 items onto terminal"
expect "$T/out-back-term.out" file_drag "copy"
expect "$T/out-back-editor.out" tree_drag "returned: 3 items onto editor"
expect "$T/out-back-editor.out" file_drag "copy"
expect "$T/out-back-esc.out" tree_drag "none"
expect "$T/out-back-esc.out" tree_drag_out "none"
expect "$T/out-back-esc.out" tabs "Home"
expect "$T/out-back-esc.out" file_ops ""
out_untyped "$T/out-back-esc.out" "garden.txt"
# Handed over, brought back, called off: nothing moved, nothing copied.
out_whole "$P2"
[ ! -e "$T/outshown2/shown" ] || failed "drag-out: a drag that was called off reached the shell: [$(cat "$T/outshown2/shown")]"
[ ! -e "$T/outtrash2" ] || failed "drag-out: a drag out of the window sent something to the Trash"

# Brought back and let go. On a folder of the tree it is the tree's own
# drop: the file moves, with the tree's words for it, and is not copied as
# a file from another app would be. With Option it copies. On the
# terminal its path is typed, and on the editor it opens. After each the
# system's drag is over. A drag from another app that follows one of ours
# is its own: copied in, never moved.
P3="$T/outproj3"
out_project "$P3"
mkdir -p "$T/outdesk"
printf 'beans by the wall\n' > "$T/outdesk/plots.txt"
cat > "$T/out-back.script" <<SCRIPT
resize 1280 800
wait 500
click @toolbar.terminal
wait 700
wait 700
text show ''
downat @sidebar.row.3 0.5 0.5
dragat @sidebar.row.2 0.5 0.5
drag -40 300
dragover @sidebar.row.1 0.5 0.5
dropat @sidebar.row.1 0.5 0.5
wait 300
wait 300
dump $T/out-back-moved.out
downat @sidebar.row.6 0.5 0.5
dragat @sidebar.row.5 0.5 0.5
drag -40 300
dragover @sidebar.row.0 0.5 0.5
flags opt
dropat @sidebar.row.0 0.5 0.5
flags -
wait 300
wait 300
dump $T/out-back-copied.out
downat @sidebar.row.7 0.5 0.5
dragat @sidebar.row.6 0.5 0.5
drag 600 -40
dump $T/out-back-term-left.out
dragover @terminal.pane.1 0.5 0.5
dropat @terminal.pane.1 0.5 0.5
wait 300
dump $T/out-back-typed.out
key 36
wait 500
downat @sidebar.row.7 0.5 0.5
dragat @sidebar.row.6 0.5 0.5
drag 600 900
dragover @text 0.5 0.5
dropat @text 0.5 0.5
wait 300
dump $T/out-back-opened.out
downat @sidebar.row.7 0.5 0.5
dragat @sidebar.row.6 0.5 0.5
drag -40 300
dragfiles $T/outdesk/plots.txt
dragover @sidebar.row.3 0.5 0.5
dump $T/out-foreign-over.out
dropat @sidebar.row.3 0.5 0.5
wait 300
wait 300
dump $T/out-foreign.out
quit
SCRIPT
HOME="$T/home/out3" SHOWN_DIR="$T/outshown3" PATH="$T/outbin:$PATH" CRC_TERMINAL_SHELL=/bin/sh \
    CRC_SELFTEST_TRASH="$T/outtrash3" CRC_SELFTEST="$T/out-back.script" "$BIN" "$P3" 2> "$T/out-back.err"
out_quiet "$T/out-back.err"
expect "$T/out-back-moved.out" file_drag "dropped"
expect "$T/out-back-moved.out" message "moved garden.txt to recipes"
expect "$T/out-back-moved.out" tree_drag "none"
expect "$T/out-back-moved.out" tree_drag_out "none"
expect "$T/out-back-moved.out" tree_keys true
expect "$T/out-back-moved.out" tree_rows "notes/|recipes/|recipes/bread.md|*>recipes/garden.txt|recipes/soup.md|anna's list.txt|journal.md"
expect "$T/out-back-moved.out" file_ops "move garden.txt > recipes/garden.txt"
out_holds "$P3/recipes/garden.txt" "tomatoes in may"
[ ! -e "$P3/garden.txt" ] || failed "drag-out: a file brought back onto a folder was copied there, not moved"
expect "$T/out-back-copied.out" file_drag "dropped"
expect "$T/out-back-copied.out" message "copied journal.md to notes"
expect "$T/out-back-copied.out" tree_drag_out "none"
expect "$T/out-back-copied.out" file_ops "move garden.txt > recipes/garden.txt|copy journal.md > notes/journal.md"
out_holds "$P3/notes/journal.md" "a quiet day"
out_holds "$P3/journal.md" "a quiet day"
expect "$T/out-back-term-left.out" tree_drag_out "outside=copy inside=copy|move items=anna's list.txt"
expect "$T/out-back-typed.out" file_drag "dropped"
expect "$T/out-back-typed.out" tree_drag "none"
expect "$T/out-back-typed.out" tree_drag_out "none"
expect "$T/out-back-typed.out" tree_keys false
expect_terminal "$T/out-back-typed.out" "open=true focus=true "
out_typed "$T/out-back-typed.out" "show '''$P3/anna'\\\\''s list.txt'"
if [ "$(cat "$T/outshown3/shown" 2>/dev/null)" != "<$P3/anna's list.txt>" ]; then
    failed "drag-out: the shell was given [$(cat "$T/outshown3/shown" 2>&1)], expected [<$P3/anna's list.txt>]"
fi
expect "$T/out-back-opened.out" file_drag "dropped"
expect "$T/out-back-opened.out" tabs "anna's list.txt"
expect "$T/out-back-opened.out" tree_drag_out "none"
expect_line "$T/out-back-opened.out" 1 "milk and eggs"
expect "$T/out-back-opened.out" file_ops "move garden.txt > recipes/garden.txt|copy journal.md > notes/journal.md"
out_holds "$P3/anna's list.txt" "milk and eggs"
expect "$T/out-foreign-over.out" tree_drag "from outside: plots.txt onto recipes"
expect "$T/out-foreign-over.out" file_drag "copy"
expect "$T/out-foreign.out" message "copied plots.txt to recipes"
expect "$T/out-foreign.out" tree_drag_out "none"
expect "$T/out-foreign.out" file_ops "move garden.txt > recipes/garden.txt|copy journal.md > notes/journal.md|copy $T/outdesk/plots.txt > recipes/plots.txt"
out_holds "$T/outdesk/plots.txt" "beans by the wall"
out_holds "$P3/anna's list.txt" "milk and eggs"
[ ! -e "$T/outtrash3" ] || failed "drag-out: a drag brought back sent something to the Trash"

# Two panes. The terminal is cut in two in Agents mode, and under the
# editor it still is. A drag held over a pane moves no keyboard; let go,
# its path is typed into the session of that pane and no other, and that
# pane has the keyboard: the left one, then the right one.
P4="$T/outproj4"
out_project "$P4"
cat > "$T/out-panes.script" <<SCRIPT
resize 1280 800
wait 500
key 0 cmd,shift A
wait 900
key 42 cmd \\
wait 900
key 0 cmd,shift A
wait 700
click @toolbar.terminal
wait 700
dump $T/out-panes.out
downat @sidebar.row.3 0.5 0.5
dragat @sidebar.row.2 0.5 0.5
dragat @terminal.pane.1 0.5 0.5
dump $T/out-pane1-held.out
upat @terminal.pane.1 0.5 0.5
wait 300
dump $T/out-pane1.out
downat @sidebar.row.4 0.5 0.5
dragat @sidebar.row.2 0.5 0.5
dragat @terminal.pane.2 0.5 0.5
upat @terminal.pane.2 0.5 0.5
wait 300
dump $T/out-pane2.out
quit
SCRIPT
HOME="$T/home/out4" CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST_TRASH="$T/outtrash4" \
    CRC_SELFTEST="$T/out-panes.script" "$BIN" "$P4" 2> "$T/out-panes.err"
out_quiet "$T/out-panes.err"
expect "$T/out-panes.out" terminal_tabs "sh,*sh panes=h(0.5,sh,*sh) scrolled=no drag=-"
expect "$T/out-pane1-held.out" tree_drag "garden.txt onto terminal"
expect "$T/out-pane1-held.out" terminal_tabs "sh,*sh panes=h(0.5,sh,*sh) scrolled=no drag=-"
expect "$T/out-pane1.out" terminal_tabs "*sh,sh panes=h(0.5,*sh,sh) scrolled=no drag=-"
expect_terminal "$T/out-pane1.out" "open=true focus=true "
out_typed "$T/out-pane1.out" "'$P4/garden.txt'"
expect "$T/out-pane2.out" terminal_tabs "sh,*sh panes=h(0.5,sh,*sh) scrolled=no drag=-"
expect_terminal "$T/out-pane2.out" "open=true focus=true "
out_typed "$T/out-pane2.out" "'$P4/journal.md'"
out_untyped "$T/out-pane2.out" "garden.txt"
expect "$T/out-pane2.out" file_ops ""
out_whole "$P4"
