# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh explorer-undo.

# ---- undo and redo of file operations in the Explorer -----------------------
# While the tree has the keyboard, Cmd-Z takes back the last thing done to
# the files and Cmd-Shift-Z does it again: a new file or folder, a rename,
# a move, a copy, a duplicate, a move to the Trash, a drop that replaced an
# item. One thing done is one undo, however many items it touched. Nothing
# is deleted on the way: what an operation made goes to the Trash, and what
# it sent there comes back. With the document focused Cmd-Z is the
# document's, as ever. Every dump lists the operations an undo can still
# take back (`file_ops:`) and what Edit > Undo and Redo are called
# (`edit_undo:`, `edit_redo:`: `(off)` after one that cannot be chosen,
# and plain `Undo` and `Redo` when they are the document's). The Trash is
# a folder of the fixture: no test instance has another.
undo_project() {
    mkdir -p "$1/recipes" "$1/trips"
    printf 'flour, water, salt\n' > "$1/recipes/bread.md"
    printf 'leeks and potatoes\n' > "$1/recipes/soup.md"
    printf 'tram 28\n' > "$1/trips/lisbon.md"
    printf 'tomatoes in may\n' > "$1/garden.txt"
    printf 'oat milk\n' > "$1/groceries.txt"
    printf 'a quiet day\n' > "$1/journal.md"
    printf 'dear anna\n' > "$1/letters.txt"
}
# undo_holds <file> <text>: the file is there with this one line in it.
undo_holds() {
    if [ "$(cat "$1" 2>/dev/null)" != "$2" ]; then
        failed "undo: $1 holds [$(cat "$1" 2>&1)], expected [$2]"
    fi
}
# undo_absent <path>: nothing is there.
undo_absent() {
    if [ -e "$1" ] || [ -L "$1" ]; then
        failed "undo: $1 is there, and should not be"
    fi
}
# undo_kept <path under the run's folder>: reason:size of each version
# local history keeps for it, oldest first.
undo_kept() {
    /usr/bin/sqlite3 "$CRC_HISTORY_DB" "SELECT group_concat(row, ' ') FROM (
        SELECT s.reason || ':' || b.size AS row FROM snapshots s
        JOIN files f ON f.id = s.file JOIN blobs b ON b.id = s.blob
        WHERE f.path LIKE '%/$1' ORDER BY s.id)" 2>&1
}
# undo_quiet <stderr file>: takes out the one line that is the system's and
# not crc's. Its pasteboard says it whenever a process that is not
# sandboxed reads a file's URL from it, and the read succeeds all the same.
undo_quiet() {
    grep -v '^sandbox_extension_consume failed: 1 (Operation not permitted)$' "$1" > "$1.kept" || true
    mv -f "$1.kept" "$1"
}

# A new file, and a rename. The file just made has the keyboard: what is
# typed goes into it, and Cmd-Z there takes the typing back and no file.
# With the tree focused Cmd-Z takes the file itself back, to the Trash and
# with its tab, and Cmd-Shift-Z brings it back. A rename is undone to the
# old name and redone to the new one, the row selected each time. Then the
# old name is taken by something else: the rename cannot be undone, says
# why, touches neither file, and is not in the way of the step before it.
undo_project "$T/undoproj"
cat > "$T/undo.script" <<SCRIPT
wait 500
click @sidebar.action.0
wait 200
text plots.md
key 36
wait 800
text beans
dump $T/undo-typed.out
key 6 cmd z
wait 200
dump $T/undo-document.out
click 150 650
dump $T/undo-tree.out
key 6 cmd z
wait 300
wait 300
dump $T/undo-create.out
key 6 cmd,shift z
wait 300
wait 300
dump $T/undo-create-redo.out
menuat @sidebar.row.2 0.5 0.5
action renameProjectItem:
wait 200
text yard
key 36
wait 700
dump $T/undo-renamed.out
key 6 cmd z
wait 300
wait 300
dump $T/undo-rename.out
key 6 cmd,shift z
wait 300
wait 300
dump $T/undo-rename-redo.out
write $T/undoproj/garden.txt another garden
key 6 cmd z
wait 300
wait 300
dump $T/undo-rename-taken.out
key 6 cmd z
wait 300
wait 300
dump $T/undo-create-after.out
key 6 cmd z
wait 200
dump $T/undo-nothing.out
quit
SCRIPT
CRC_SELFTEST_TRASH="$T/undotrash" CRC_SELFTEST="$T/undo.script" "$BIN" "$T/undoproj" 2> "$T/undo.err"
expect "$T/undo-typed.out" tabs "plots.md"
expect_line "$T/undo-typed.out" 1 "beans"
expect "$T/undo-typed.out" tree_keys false
expect "$T/undo-typed.out" edit_undo "Undo"
expect "$T/undo-typed.out" edit_redo "Redo (off)"
expect "$T/undo-typed.out" file_ops "new-file - > plots.md"
# Cmd-Z in the document: typing goes, the file stays, and so does what
# was done to the files.
expect_line "$T/undo-document.out" 1 ""
expect "$T/undo-document.out" tabs "plots.md"
expect "$T/undo-document.out" file_ops "new-file - > plots.md"
expect "$T/undo-document.out" edit_undo "Undo (off)"
expect "$T/undo-document.out" edit_redo "Redo"
expect "$T/undo-tree.out" tree_keys true
expect "$T/undo-tree.out" edit_undo "Undo New File"
expect "$T/undo-tree.out" edit_redo "Redo (off)"
expect "$T/undo-create.out" message "undid new file plots.md"
expect "$T/undo-create.out" tabs "Home"
expect "$T/undo-create.out" tree_keys true
expect "$T/undo-create.out" tree_rows "recipes/|trips/|garden.txt|groceries.txt|journal.md|letters.txt"
expect "$T/undo-create.out" file_ops ""
expect "$T/undo-create.out" edit_undo "Undo (off)"
expect "$T/undo-create.out" edit_redo "Redo New File"
expect "$T/undo-create-redo.out" message "redid new file plots.md"
expect "$T/undo-create-redo.out" tree_keys true
expect "$T/undo-create-redo.out" tree_rows "recipes/|trips/|garden.txt|groceries.txt|journal.md|letters.txt|*>plots.md"
expect "$T/undo-create-redo.out" file_ops "new-file - > plots.md"
expect "$T/undo-create-redo.out" edit_undo "Undo New File"
expect "$T/undo-create-redo.out" edit_redo "Redo (off)"
expect "$T/undo-renamed.out" tree_keys true
expect "$T/undo-renamed.out" tree_rows "recipes/|trips/|groceries.txt|journal.md|letters.txt|plots.md|*>yard.txt"
expect "$T/undo-renamed.out" file_ops "new-file - > plots.md|rename garden.txt > yard.txt"
expect "$T/undo-renamed.out" edit_undo "Undo Rename"
expect "$T/undo-rename.out" message "undid rename to yard.txt"
expect "$T/undo-rename.out" tree_keys true
expect "$T/undo-rename.out" tree_rows "recipes/|trips/|*>garden.txt|groceries.txt|journal.md|letters.txt|plots.md"
expect "$T/undo-rename.out" file_ops "new-file - > plots.md"
expect "$T/undo-rename.out" edit_undo "Undo New File"
expect "$T/undo-rename.out" edit_redo "Redo Rename"
expect "$T/undo-rename-redo.out" message "redid rename to yard.txt"
expect "$T/undo-rename-redo.out" tree_rows "recipes/|trips/|groceries.txt|journal.md|letters.txt|plots.md|*>yard.txt"
expect "$T/undo-rename-redo.out" file_ops "new-file - > plots.md|rename garden.txt > yard.txt"
expect "$T/undo-rename-redo.out" edit_redo "Redo (off)"
# The old name has been taken: nothing is written over, the step is gone,
# and the one before it is next.
expect "$T/undo-rename-taken.out" message "could not undo rename to yard.txt: yard.txt was left alone: an item named garden.txt is in the way"
expect "$T/undo-rename-taken.out" file_ops "new-file - > plots.md"
expect "$T/undo-rename-taken.out" edit_undo "Undo New File"
expect "$T/undo-rename-taken.out" edit_redo "Redo (off)"
expect "$T/undo-create-after.out" message "undid new file plots.md"
expect "$T/undo-create-after.out" file_ops ""
expect "$T/undo-nothing.out" message "nothing to undo"
undo_holds "$T/undoproj/garden.txt" "another garden"
undo_holds "$T/undoproj/yard.txt" "tomatoes in may"
undo_absent "$T/undoproj/plots.md"
# Taken back, the new file is in the Trash: never deleted.
[ -f "$T/undotrash/plots.md" ] || failed "undo: the new file taken back is not in the Trash: [$(ls "$T/undotrash" 2>&1 | tr '\n' ' ')]"
[ "$(ls "$T/undotrash" | wc -l | tr -d ' ')" = 1 ] || failed "undo: the Trash holds [$(ls "$T/undotrash" | tr '\n' ' ')], expected the one new file"

# Several items moved by a drag, two of them open in tabs with versions
# kept in local history: one Cmd-Z brings all three back, selected, their
# tabs and their history with them, and nothing is left under the paths
# they were moved to. Then the document is clicked: Cmd-Z there takes back
# what was typed and touches no file, and what is saved goes to the file
# where it is again.
undo_project "$T/undoproj2"
cat > "$T/undo-drag.script" <<SCRIPT
wait 500
key 35 cmd p
text journal.md
key 36
wait 300
text a
key 1 cmd s
wait 400
key 35 cmd p
text garden.txt
key 36
wait 300
text b
key 1 cmd s
wait 400
click 150 650
clickat @sidebar.row.2 0.5 0.5 cmd
clickat @sidebar.row.3 0.5 0.5 cmd
clickat @sidebar.row.4 0.5 0.5 cmd
downat @sidebar.row.3 0.5 0.5
dragat @sidebar.row.0 0.5 0.5
upat @sidebar.row.0 0.5 0.5
wait 300
wait 300
dump $T/undo-dragged.out
key 6 cmd z
wait 300
wait 300
dump $T/undo-drag.out
clickat @text 0.5 0.5
text c
dump $T/undo-drag-typed.out
key 6 cmd z
wait 200
dump $T/undo-drag-document.out
text c
key 1 cmd s
wait 400
dump $T/undo-drag-saved.out
quit
SCRIPT
CRC_SELFTEST_TRASH="$T/undotrash2" CRC_SELFTEST="$T/undo-drag.script" "$BIN" "$T/undoproj2" 2> "$T/undo-drag.err"
expect "$T/undo-dragged.out" message "moved 3 items to recipes"
expect "$T/undo-dragged.out" file_ops "move garden.txt > recipes/garden.txt|move groceries.txt > recipes/groceries.txt|move journal.md > recipes/journal.md"
expect "$T/undo-dragged.out" edit_undo "Undo Move of 3 Items"
expect "$T/undo-drag.out" message "undid move of 3 items"
expect "$T/undo-drag.out" tree_keys true
expect "$T/undo-drag.out" tree_rows "recipes/|recipes/bread.md|recipes/soup.md|trips/|*>garden.txt|*groceries.txt|*journal.md|letters.txt"
expect "$T/undo-drag.out" file_ops ""
expect "$T/undo-drag.out" edit_undo "Undo (off)"
expect "$T/undo-drag.out" edit_redo "Redo Move of 3 Items"
# The tabs came back with their files: still two, on the names they had.
expect "$T/undo-drag.out" tabs "journal.md | garden.txt"
expect "$T/undo-drag.out" dirty false
expect "$T/undo-drag-typed.out" tree_keys false
expect "$T/undo-drag-typed.out" dirty true
expect "$T/undo-drag-typed.out" edit_undo "Undo"
expect "$T/undo-drag-typed.out" edit_redo "Redo (off)"
# Cmd-Z in the document: the typing is gone, and no file has moved.
expect "$T/undo-drag-document.out" dirty false
expect "$T/undo-drag-document.out" edit_undo "Undo"
expect "$T/undo-drag-document.out" edit_redo "Redo"
expect "$T/undo-drag-document.out" file_ops ""
expect "$T/undo-drag-document.out" tree_rows "recipes/|recipes/bread.md|recipes/soup.md|trips/|*>garden.txt|*groceries.txt|*journal.md|letters.txt"
expect "$T/undo-drag-document.out" message "undid move of 3 items"
expect "$T/undo-drag-saved.out" dirty false
undo_holds "$T/undoproj2/journal.md" "aa quiet day"
undo_holds "$T/undoproj2/groceries.txt" "oat milk"
grep -q '^btomatoes in may$' "$T/undoproj2/garden.txt" 2>/dev/null \
    || failed "undo: garden.txt is [$(cat "$T/undoproj2/garden.txt" 2>&1)] after the move was undone"
grep -q c "$T/undoproj2/garden.txt" 2>/dev/null \
    || failed "undo: what was typed after the undo was not saved to garden.txt where it is again: the tab did not follow it"
for undo_gone in garden.txt groceries.txt journal.md; do
    undo_absent "$T/undoproj2/recipes/$undo_gone"
done
[ "$(undo_kept undoproj2/journal.md)" = "1:12 2:13" ] \
    || failed "undo: history of journal.md where it is again is [$(undo_kept undoproj2/journal.md)], expected [1:12 2:13]"
case "$(undo_kept undoproj2/garden.txt)" in
    "1:16 2:17"*) ;;
    *) failed "undo: history of garden.txt where it is again is [$(undo_kept undoproj2/garden.txt)], expected it to start with [1:16 2:17]" ;;
esac
[ -z "$(undo_kept undoproj2/recipes/journal.md)" ] \
    || failed "undo: history was left where journal.md was moved to: [$(undo_kept undoproj2/recipes/journal.md)]"
[ -z "$(undo_kept undoproj2/recipes/garden.txt)" ] \
    || failed "undo: history was left where garden.txt was moved to: [$(undo_kept undoproj2/recipes/garden.txt)]"
undo_absent "$T/undotrash2"

# A place taken meanwhile. Two files are dragged into a folder, and then
# something else takes the name one of them had: Cmd-Z brings the other
# back, says which one was left alone and why, and writes over nothing.
# What was undone can be done again, and that is the one item.
undo_project "$T/undoproj3"
cat > "$T/undo-taken.script" <<SCRIPT
wait 500
click 150 650
clickat @sidebar.row.2 0.5 0.5 cmd
clickat @sidebar.row.3 0.5 0.5 cmd
downat @sidebar.row.2 0.5 0.5
dragat @sidebar.row.1 0.5 0.5
upat @sidebar.row.1 0.5 0.5
wait 300
wait 300
dump $T/undo-taken-dragged.out
write $T/undoproj3/garden.txt another garden
key 6 cmd z
wait 300
wait 300
dump $T/undo-taken.out
key 6 cmd,shift z
wait 300
wait 300
dump $T/undo-taken-redo.out
quit
SCRIPT
CRC_SELFTEST_TRASH="$T/undotrash3" CRC_SELFTEST="$T/undo-taken.script" "$BIN" "$T/undoproj3" 2> "$T/undo-taken.err"
expect "$T/undo-taken-dragged.out" message "moved 2 items to trips"
expect "$T/undo-taken-dragged.out" file_ops "move garden.txt > trips/garden.txt|move groceries.txt > trips/groceries.txt"
expect "$T/undo-taken.out" message "undid move of 1 of 2 items; garden.txt was left alone: an item named garden.txt is in the way"
expect "$T/undo-taken.out" tree_keys true
expect "$T/undo-taken.out" file_ops ""
expect "$T/undo-taken.out" edit_undo "Undo (off)"
expect "$T/undo-taken.out" edit_redo "Redo Move of groceries.txt"
expect "$T/undo-taken-redo.out" message "redid move of groceries.txt"
expect "$T/undo-taken-redo.out" file_ops "move groceries.txt > trips/groceries.txt"
expect "$T/undo-taken-redo.out" edit_undo "Undo Move of groceries.txt"
expect "$T/undo-taken-redo.out" edit_redo "Redo (off)"
undo_holds "$T/undoproj3/garden.txt" "another garden"
undo_holds "$T/undoproj3/trips/garden.txt" "tomatoes in may"
undo_holds "$T/undoproj3/trips/groceries.txt" "oat milk"
undo_absent "$T/undoproj3/groceries.txt"
undo_absent "$T/undotrash3"

# A copy by paste, a duplicate, a move to the Trash and a drop that
# replaced an item. Taken back, a copy and a duplicate go to the Trash and
# the original is not touched. Two items moved to the Trash come back
# together, go again and come back again. A file dropped from outside in
# place of one of the same name: undone, the old one is back from the
# Trash and the new one is there instead, and the tab open on the file
# shows what is there each time. With unsaved work in that tab the undo is
# refused whole, and stays to be done.
undo_project "$T/undoproj4"
mkdir -p "$T/undodesk"
printf 'pumpkin and sage\n' > "$T/undodesk/soup.md"
cat > "$T/undo-trash.script" <<SCRIPT
wait 500
click 150 650
clickat @sidebar.row.2 0.5 0.5 cmd
key 8 cmd c
key 9 cmd v
wait 300
wait 300
dump $T/undo-pasted.out
key 6 cmd z
wait 300
wait 300
dump $T/undo-paste.out
click 150 650
clickat @sidebar.row.5 0.5 0.5 cmd
action duplicateProjectItem:
wait 300
wait 300
dump $T/undo-duplicated.out
key 6 cmd z
wait 300
wait 300
dump $T/undo-duplicate.out
click 150 650
clickat @sidebar.row.3 0.5 0.5 cmd
clickat @sidebar.row.4 0.5 0.5 cmd
key 51 cmd
wait 300
wait 300
dump $T/undo-trashed.out
key 6 cmd z
wait 300
wait 300
dump $T/undo-trash.out
key 6 cmd,shift z
wait 300
wait 300
dump $T/undo-trash-redo.out
key 6 cmd z
wait 300
wait 300
dump $T/undo-trash-again.out
key 35 cmd p
text soup.md
key 36
wait 300
dragfiles $T/undodesk/soup.md
dragover @sidebar.row.0 0.5 0.5
dropat @sidebar.row.0 0.5 0.5
wait 300
wait 300
dump $T/undo-replaced.out
key 6 cmd z
wait 300
wait 300
dump $T/undo-replace.out
key 6 cmd,shift z
wait 300
wait 300
dump $T/undo-replace-redo.out
clickat @text 0.5 0.5
text x
click 150 650
key 6 cmd z
wait 300
dump $T/undo-unsaved.out
quit
SCRIPT
CRC_TRASH_ANSWER=move CRC_DROP_ANSWER=replace CRC_SELFTEST_TRASH="$T/undotrash4" CRC_SELFTEST="$T/undo-trash.script" "$BIN" "$T/undoproj4" 2> "$T/undo-trash.err"
undo_quiet "$T/undo-trash.err"
expect "$T/undo-pasted.out" message "pasted garden-copy.txt"
expect "$T/undo-pasted.out" file_ops "copy garden.txt > garden-copy.txt"
expect "$T/undo-pasted.out" edit_undo "Undo Copy of garden-copy.txt"
expect "$T/undo-paste.out" message "undid copy of garden-copy.txt"
expect "$T/undo-paste.out" tree_keys true
expect "$T/undo-paste.out" tree_rows "recipes/|trips/|garden.txt|groceries.txt|journal.md|letters.txt"
expect "$T/undo-paste.out" file_ops ""
expect "$T/undo-paste.out" edit_redo "Redo Copy of garden-copy.txt"
expect "$T/undo-duplicated.out" message "duplicated letters-copy.txt"
expect "$T/undo-duplicated.out" file_ops "duplicate letters.txt > letters-copy.txt"
expect "$T/undo-duplicated.out" edit_undo "Undo Duplicate of letters-copy.txt"
# Something new was done: the copy taken back can no longer be done again.
expect "$T/undo-duplicated.out" edit_redo "Redo (off)"
expect "$T/undo-duplicate.out" message "undid duplicate of letters-copy.txt"
expect "$T/undo-duplicate.out" tree_rows "recipes/|trips/|garden.txt|groceries.txt|journal.md|letters.txt"
expect "$T/undo-duplicate.out" file_ops ""
expect "$T/undo-trashed.out" message "moved 2 items to Trash"
expect "$T/undo-trashed.out" tree_rows "recipes/|trips/|garden.txt|letters.txt"
expect "$T/undo-trashed.out" file_ops "trash groceries.txt > -|trash journal.md > -"
expect "$T/undo-trashed.out" edit_undo "Undo Move to Trash of 2 Items"
expect "$T/undo-trash.out" message "undid move to Trash of 2 items"
expect "$T/undo-trash.out" tree_keys true
expect "$T/undo-trash.out" tree_rows "recipes/|trips/|garden.txt|*>groceries.txt|*journal.md|letters.txt"
expect "$T/undo-trash.out" file_ops ""
expect "$T/undo-trash.out" edit_redo "Redo Move to Trash of 2 Items"
expect "$T/undo-trash-redo.out" message "redid move to Trash of 2 items"
expect "$T/undo-trash-redo.out" tree_rows "recipes/|trips/|garden.txt|letters.txt"
expect "$T/undo-trash-redo.out" file_ops "trash groceries.txt > -|trash journal.md > -"
expect "$T/undo-trash-again.out" message "undid move to Trash of 2 items"
expect "$T/undo-trash-again.out" tree_rows "recipes/|trips/|garden.txt|*>groceries.txt|*journal.md|letters.txt"
expect "$T/undo-replaced.out" message "copied soup.md to recipes; the item it replaced is in the Trash"
expect "$T/undo-replaced.out" tree_keys true
expect "$T/undo-replaced.out" file_ops "replace $T/undodesk/soup.md > recipes/soup.md trashed $T/undotrash4/soup.md"
expect "$T/undo-replaced.out" edit_undo "Undo Replace of soup.md"
expect "$T/undo-replaced.out" tabs "soup.md"
expect_line "$T/undo-replaced.out" 1 "pumpkin and sage"
# Undone: the old text is back under the name, in the tab as well.
expect "$T/undo-replace.out" message "undid replace of soup.md"
expect "$T/undo-replace.out" tree_keys true
expect "$T/undo-replace.out" tree_rows "recipes/|recipes/bread.md|*>recipes/soup.md|trips/|garden.txt|groceries.txt|journal.md|letters.txt"
expect "$T/undo-replace.out" file_ops ""
expect "$T/undo-replace.out" edit_redo "Redo Replace of soup.md"
expect "$T/undo-replace.out" tabs "soup.md"
expect_line "$T/undo-replace.out" 1 "leeks and potatoes"
expect "$T/undo-replace.out" dirty false
expect "$T/undo-replace-redo.out" message "redid replace of soup.md"
expect "$T/undo-replace-redo.out" file_ops "replace $T/undodesk/soup.md > recipes/soup.md trashed $T/undotrash4/soup.md"
expect_line "$T/undo-replace-redo.out" 1 "pumpkin and sage"
expect "$T/undo-replace-redo.out" dirty false
# Unsaved work in the tab on the file that would go to the Trash: refused,
# nothing moved, and the step is still there to be undone.
expect "$T/undo-unsaved.out" message "save the affected tabs before undoing this"
expect "$T/undo-unsaved.out" file_ops "replace $T/undodesk/soup.md > recipes/soup.md trashed $T/undotrash4/soup.md"
expect "$T/undo-unsaved.out" edit_undo "Undo Replace of soup.md"
expect "$T/undo-unsaved.out" dirty true
undo_holds "$T/undoproj4/recipes/soup.md" "pumpkin and sage"
undo_holds "$T/undotrash4/soup.md" "leeks and potatoes"
undo_holds "$T/undodesk/soup.md" "pumpkin and sage"
undo_holds "$T/undoproj4/garden.txt" "tomatoes in may"
undo_holds "$T/undoproj4/groceries.txt" "oat milk"
undo_holds "$T/undoproj4/journal.md" "a quiet day"
undo_holds "$T/undoproj4/letters.txt" "dear anna"
undo_absent "$T/undoproj4/garden-copy.txt"
undo_absent "$T/undoproj4/letters-copy.txt"
# What the undos took back is in the Trash, whole: the copy, the
# duplicate, and nothing else beside the item that was replaced.
undo_holds "$T/undotrash4/garden-copy.txt" "tomatoes in may"
undo_holds "$T/undotrash4/letters-copy.txt" "dear anna"
[ "$(ls "$T/undotrash4" | wc -l | tr -d ' ')" = 3 ] || failed "undo: the Trash holds [$(ls "$T/undotrash4" | tr '\n' ' ')], expected the copy, the duplicate and the item replaced"
if [ -n "$(find "$T/undoproj4" -name '.crc-copy-*' 2>/dev/null)" ]; then
    failed "undo: a copy left the folder it was built in behind"
fi

# While a copy is running nothing is undone: the copy is held (it waits
# for a file that never comes), Cmd-Z says so and the rename before it
# stays done. Escape stops the copy, and then the rename is undone.
undo_project "$T/undoproj5"
cat > "$T/undo-busy.script" <<SCRIPT
wait 500
menuat @sidebar.row.2 0.5 0.5
action renameProjectItem:
wait 200
text yard
key 36
wait 700
key 8 cmd c
key 9 cmd v
wait 300
key 6 cmd z
wait 200
dump $T/undo-busy.out
key 53
wait 300
wait 300
dump $T/undo-busy-stopped.out
key 6 cmd z
wait 300
wait 300
dump $T/undo-busy-after.out
quit
SCRIPT
CRC_COPY_GATE="$T/undogate/open" CRC_SELFTEST_TRASH="$T/undotrash5" CRC_SELFTEST="$T/undo-busy.script" "$BIN" "$T/undoproj5" 2> "$T/undo-busy.err"
undo_quiet "$T/undo-busy.err"
expect "$T/undo-busy.out" file_job "copying 1"
expect "$T/undo-busy.out" message "a copy is still running: Escape in the Explorer stops it"
expect "$T/undo-busy.out" file_ops "rename garden.txt > yard.txt"
expect "$T/undo-busy-stopped.out" file_job "none"
expect "$T/undo-busy-stopped.out" file_ops "rename garden.txt > yard.txt"
expect "$T/undo-busy-after.out" message "undid rename to yard.txt"
expect "$T/undo-busy-after.out" file_ops ""
expect "$T/undo-busy-after.out" tree_rows "recipes/|trips/|*>garden.txt|groceries.txt|journal.md|letters.txt"
undo_holds "$T/undoproj5/garden.txt" "tomatoes in may"
undo_absent "$T/undoproj5/yard.txt"
undo_absent "$T/undoproj5/yard-copy.txt"
undo_absent "$T/undotrash5"
