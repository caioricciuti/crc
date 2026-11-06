# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh explorer-selection.

# ---- selecting in the Explorer ---------------------------------------------
# The tree selects many rows and takes the keyboard. Every dump lists the
# rows by their path in the project: a folder ends in a slash, a selected
# row is starred, the cursor's row follows a `>`. Rows are clicked by name
# (`sidebar.row.N`, from 0), so the layout may move without the script
# noticing. Move to Trash is asked for but never done: a test instance says
# what it would ask, and nothing reaches the real Trash.
#
# A click on a file opens it and the file has the keyboard, as it always
# had. The tree takes it on a folder, a Command or Shift click, a
# right-click, the empty area under the rows, and View > Focus Explorer,
# which has no key and is run here from the palette's commands.
pick_focus='key 35 cmd p
text >focus explorer
key 36
wait 200'
pick_project() {
    mkdir -p "$1/recipes" "$1/trips"
    printf 'flour, water, salt\n' > "$1/recipes/bread.md"
    printf 'leeks and potatoes\n' > "$1/recipes/soup.md"
    printf 'tram 28\n' > "$1/trips/lisbon.md"
    printf 'tomatoes in may\n' > "$1/garden.txt"
    printf 'oat milk\n' > "$1/groceries.txt"
    printf 'a quiet day\n' > "$1/journal.md"
    printf 'dear anna\n' > "$1/letters.txt"
}
# The six rows a fresh project shows, as a dump lists them, with <marks> in
# front of the rows named: `pick_rows '*>' journal.md` stars journal.md and
# puts the cursor on it.
pick_rows() {
    local out="" row mark i name
    for row in recipes/ trips/ garden.txt groceries.txt journal.md letters.txt; do
        mark=""
        i=1
        while [ "$i" -lt "$#" ]; do
            name=$((i + 1))
            if [ "${!name}" = "$row" ]; then mark=${!i}; fi
            i=$((i + 2))
        done
        out="$out${out:+|}$mark$row"
    done
    printf '%s' "$out"
}

# The mouse: a click selects one row and opens it, and what is typed next
# is the file's: the tree's cursor stays. Focus Explorer puts the keys in
# the tree on that file's row, and Down then moves the cursor and leaves
# the file alone. Command adds and removes without opening, Shift selects
# from the anchor. A press on one of several selected rows changes nothing
# until it is let go without a drag. A right-click keeps a selection it
# lands in and replaces one it does not, and Rename is offered for one item
# only. A click under the rows selects nothing.
pick_project "$T/pickproj"
cat > "$T/pick.script" <<SCRIPT
wait 500
click @sidebar.row.2
wait 200
text q
key 125
dump $T/pick-click.out
$pick_focus
dump $T/pick-focus.out
key 125
dump $T/pick-focus-down.out
key 126
clickat @sidebar.row.4 0.5 0.5 cmd
dump $T/pick-cmd.out
clickat @sidebar.row.5 0.5 0.5 shift
dump $T/pick-shift.out
downat @sidebar.row.4 0.5 0.5
dump $T/pick-held.out
upat @sidebar.row.4 0.5 0.5
wait 200
dump $T/pick-released.out
clickat @sidebar.row.3 0.5 0.5 cmd
menuat @sidebar.row.4 0.5 0.5
dump $T/pick-menu-many.out
menuat @sidebar.row.5 0.5 0.5
dump $T/pick-menu-one.out
clickat @sidebar.row.4 0.5 0.5 cmd
clickat @sidebar.row.4 0.5 0.5 cmd
dump $T/pick-cmd-off.out
click 150 500
dump $T/pick-blank.out
quit
SCRIPT
CRC_SELFTEST="$T/pick.script" "$BIN" "$T/pickproj" 2> "$T/pick.err"
expect "$T/pick-click.out" tabs "garden.txt"
expect "$T/pick-click.out" tree_keys false
expect "$T/pick-click.out" tree_rows "$(pick_rows '*>' garden.txt)"
expect_line "$T/pick-click.out" 1 "qtomatoes in may"
expect "$T/pick-click.out" cursor "2:1"
expect "$T/pick-focus.out" tree_keys true
expect "$T/pick-focus.out" tree_rows "$(pick_rows '*>' garden.txt)"
expect "$T/pick-focus-down.out" tree_keys true
expect "$T/pick-focus-down.out" tree_rows "$(pick_rows '*>' groceries.txt)"
expect "$T/pick-focus-down.out" tabs "garden.txt"
expect_line "$T/pick-focus-down.out" 1 "qtomatoes in may"
expect "$T/pick-focus-down.out" cursor "2:1"
expect "$T/pick-cmd.out" tabs "garden.txt"
expect "$T/pick-cmd.out" tree_rows "$(pick_rows '*' garden.txt '*>' journal.md)"
expect "$T/pick-shift.out" tabs "garden.txt"
expect "$T/pick-shift.out" tree_rows "$(pick_rows '*' journal.md '*>' letters.txt)"
expect "$T/pick-held.out" tabs "garden.txt"
expect "$T/pick-held.out" tree_rows "$(pick_rows '*' journal.md '*>' letters.txt)"
expect "$T/pick-released.out" tabs "garden.txt | journal.md"
expect "$T/pick-released.out" tree_rows "$(pick_rows '*>' journal.md)"
expect "$T/pick-released.out" tree_keys false
expect "$T/pick-menu-many.out" tree_rows "$(pick_rows '*' groceries.txt '*>' journal.md)"
expect "$T/pick-menu-many.out" message "menu: New File…|New Folder…|Cut|Copy|Paste|Duplicate|Copy Path|Copy Relative Path|Reveal in Finder|Move to Trash"
expect "$T/pick-menu-many.out" tree_keys true
expect "$T/pick-menu-one.out" tree_rows "$(pick_rows '*>' letters.txt)"
expect "$T/pick-menu-one.out" message "menu: New File…|New Folder…|Cut|Copy|Paste|Duplicate|Copy Path|Copy Relative Path|Reveal in Finder|Rename…|Move to Trash"
# Taken out again with Command, the cursor goes back to what is selected.
expect "$T/pick-cmd-off.out" tree_rows "$(pick_rows '*>' letters.txt)"
expect "$T/pick-blank.out" tree_rows "$(pick_rows)"
expect "$T/pick-blank.out" tree_acts_on ""
expect "$T/pick-blank.out" tree_keys true
expect "$T/pick-blank.out" tabs "garden.txt | journal.md"

# The keyboard, once the tree has it: the arrows move the cursor
# and Shift extends, Escape reduces a selection to the cursor's row, Right
# and Left open and close a folder and step in and out of it, typed letters
# jump to a name, Space opens and stays, Cmd-Down opens and hands the keys
# to the file. None of it reaches the document, and once the document has
# the keys, typing and the arrows no longer move the tree.
pick_project "$T/keysproj"
cat > "$T/keys.script" <<SCRIPT
wait 500
click @sidebar.row.2
wait 200
$pick_focus
key 125
key 125
dump $T/keys-down.out
key 126 shift
dump $T/keys-shift.out
key 53
dump $T/keys-reduce.out
key 115
key 124
wait 300
dump $T/keys-open.out
key 124
key 125
dump $T/keys-inside.out
key 123
dump $T/keys-parent.out
key 123
dump $T/keys-closed.out
key 119
dump $T/keys-end.out
text jo
dump $T/keys-typed.out
wait 1200
text g
text g
dump $T/keys-stepped.out
wait 1200
key 49
wait 200
dump $T/keys-space.out
key 125
key 125 cmd
wait 200
dump $T/keys-cmd-down.out
text x
key 125
key 126
dump $T/keys-editor.out
quit
SCRIPT
CRC_SELFTEST="$T/keys.script" "$BIN" "$T/keysproj" 2> "$T/keys.err"
expect "$T/keys-down.out" tree_rows "$(pick_rows '*>' journal.md)"
# The arrows moved the tree, not the caret, and opened nothing.
expect "$T/keys-down.out" tabs "garden.txt"
expect "$T/keys-down.out" cursor "1:1"
expect "$T/keys-shift.out" tree_rows "$(pick_rows '*>' groceries.txt '*' journal.md)"
expect "$T/keys-reduce.out" tree_rows "$(pick_rows '*>' groceries.txt)"
expect "$T/keys-reduce.out" tree_keys true
expect "$T/keys-open.out" tree_rows "*>recipes/|recipes/bread.md|recipes/soup.md|trips/|garden.txt|groceries.txt|journal.md|letters.txt"
expect "$T/keys-inside.out" tree_rows "recipes/|recipes/bread.md|*>recipes/soup.md|trips/|garden.txt|groceries.txt|journal.md|letters.txt"
expect "$T/keys-parent.out" tree_rows "*>recipes/|recipes/bread.md|recipes/soup.md|trips/|garden.txt|groceries.txt|journal.md|letters.txt"
expect "$T/keys-closed.out" tree_rows "$(pick_rows '*>' recipes/)"
expect "$T/keys-end.out" tree_rows "$(pick_rows '*>' letters.txt)"
expect "$T/keys-typed.out" tree_rows "$(pick_rows '*>' journal.md)"
expect "$T/keys-typed.out" tabs "garden.txt"
expect_line "$T/keys-typed.out" 1 "tomatoes in may"
# The same letter again steps on to the next name that starts with it.
expect "$T/keys-stepped.out" tree_rows "$(pick_rows '*>' groceries.txt)"
expect "$T/keys-space.out" tabs "garden.txt | groceries.txt"
expect "$T/keys-space.out" active 1
expect "$T/keys-space.out" tree_keys true
expect "$T/keys-cmd-down.out" tabs "garden.txt | groceries.txt | journal.md"
expect "$T/keys-cmd-down.out" active 2
expect "$T/keys-cmd-down.out" tree_keys false
expect "$T/keys-editor.out" tree_keys false
expect "$T/keys-editor.out" tree_rows "$(pick_rows '*>' journal.md)"
expect_line "$T/keys-editor.out" 1 "xa quiet day"

# A field ahead of the tree keeps its keys: with the palette or the find
# bar open, typing and the arrows are theirs and the tree does not move.
# Return renames the cursor's item in place and F2 does the same; the keys
# are back in the tree when the field closes, on the renamed item. Cmd-A
# selects every row, Cmd-Delete asks once for all of them, and Escape from
# one selected row gives the keys back to the document.
pick_project "$T/fieldproj"
cat > "$T/field.script" <<SCRIPT
wait 500
click @sidebar.row.5
wait 200
$pick_focus
dump $T/field-focus.out
key 35 cmd p
text ga
key 125
dump $T/field-palette.out
key 53
$pick_focus
key 3 cmd f
text qq
key 125
dump $T/field-find.out
key 53
$pick_focus
key 36
dump $T/field-rename.out
text notes
key 36
wait 800
dump $T/field-renamed.out
key 120
dump $T/field-f2.out
key 53
dump $T/field-f2-cancel.out
key 0 cmd a
dump $T/field-all.out
key 51 cmd
dump $T/field-trash-all.out
key 53
key 51 cmd
dump $T/field-trash-one.out
key 53
dump $T/field-escape.out
text z
dump $T/field-doc.out
quit
SCRIPT
CRC_SELFTEST="$T/field.script" "$BIN" "$T/fieldproj" 2> "$T/field.err"
expect "$T/field-focus.out" tree_keys true
expect "$T/field-focus.out" tree_rows "$(pick_rows '*>' letters.txt)"
expect "$T/field-palette.out" palette_query "ga"
expect "$T/field-palette.out" tree_keys false
expect "$T/field-palette.out" tree_rows "$(pick_rows '*>' letters.txt)"
expect "$T/field-find.out" tree_keys false
expect "$T/field-find.out" tree_rows "$(pick_rows '*>' letters.txt)"
expect_terminal "$T/field-find.out" "find=qq target=document keys=true"
expect "$T/field-rename.out" sidebar_edit "letters.txt"
expect "$T/field-renamed.out" sidebar_edit ""
expect "$T/field-renamed.out" tree_rows "recipes/|trips/|garden.txt|groceries.txt|journal.md|*>notes.txt"
expect "$T/field-renamed.out" tree_keys true
expect "$T/field-renamed.out" tabs "notes.txt"
if [ ! -f "$T/fieldproj/notes.txt" ] || [ -e "$T/fieldproj/letters.txt" ]; then
    failed "field: Return in the tree did not rename letters.txt to notes.txt"
fi
expect "$T/field-f2.out" sidebar_edit "notes.txt"
expect "$T/field-f2-cancel.out" sidebar_edit ""
expect "$T/field-f2-cancel.out" tree_keys true
expect "$T/field-all.out" tree_rows "*recipes/|*trips/|*garden.txt|*groceries.txt|*journal.md|*>notes.txt"
expect "$T/field-trash-all.out" message "would ask: Move 6 items to Trash?"
expect "$T/field-trash-one.out" message "would ask: Move “notes.txt” to Trash?"
expect "$T/field-trash-one.out" tree_rows "recipes/|trips/|garden.txt|groceries.txt|journal.md|*>notes.txt"
expect "$T/field-escape.out" tree_keys false
expect_line "$T/field-doc.out" 1 "zdear anna"
for kept in recipes trips garden.txt groceries.txt journal.md notes.txt; do
    [ -e "$T/fieldproj/$kept" ] || failed "field: $kept is gone, and nothing was to be trashed"
done

# Unsaved work in a pane without the keyboard is in the way all the same:
# neither Move to Trash nor a drag moves the folder its file is in. A folder
# selected with something inside it is acted on once, as the folder. And a
# press on one of the selected rows that turns into a drag leaves the
# selection as it was. Opening bread.md from the palette shows it in the
# tree, so its folder is open from the start.
pick_project "$T/paneguard"
cat > "$T/guard.script" <<SCRIPT
wait 500
key 35 cmd p
text bread
wait 300
key 36
wait 300
text unsaved
key 42 cmd \\
wait 100
clickat @sidebar.row.0 0.5 0.5 cmd
clickat @sidebar.row.4 0.5 0.5 cmd
dump $T/guard-nested.out
key 51 cmd
dump $T/guard-trash.out
downat @sidebar.row.0 0.5 0.5
dragat @sidebar.row.3 0.5 0.5
upat @sidebar.row.3 0.5 0.5
wait 300
dump $T/guard-drag.out
quit
SCRIPT
CRC_SELFTEST="$T/guard.script" "$BIN" "$T/paneguard" 2> "$T/guard.err"
expect "$T/guard-nested.out" panes 2
expect "$T/guard-nested.out" focused_pane 1
expect "$T/guard-nested.out" tree_rows "*recipes/|*recipes/bread.md|recipes/soup.md|trips/|*>garden.txt|groceries.txt|journal.md|letters.txt"
expect "$T/guard-nested.out" tree_acts_on "recipes|garden.txt"
expect "$T/guard-trash.out" message "would refuse: unsaved changes are open"
expect "$T/guard-drag.out" message "save the affected tabs before moving this item"
expect "$T/guard-drag.out" focused_pane 1
expect "$T/guard-drag.out" tree_acts_on "recipes|garden.txt"
if [ ! -f "$T/paneguard/recipes/bread.md" ] || [ -e "$T/paneguard/trips/recipes" ]; then
    failed "guard: a drag moved a folder with unsaved work open in another pane"
fi
