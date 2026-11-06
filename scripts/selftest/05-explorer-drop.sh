# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh explorer-drop.

# ---- files dropped on the Explorer from another app -------------------------
# Files and folders dragged in from the Finder, or from any app that drags
# files, and let go over the tree are copied into the project: onto a
# folder's row into that folder, onto a file's row into the folder the file
# is in, under the rows into the project root. They are never moved. A test
# instance plays the drag itself (`dragfiles`, `dragover`, `dropat`,
# `dragout`), with the files on a pasteboard of its own and through the
# calls the system makes, so nothing here touches what the person running
# it is dragging. Every dump says what is held over the tree (`tree_drag:`,
# `from outside:` and the folder a drop would go into, `nowhere` where it
# would do nothing), what the window answered the drag (`file_drag:`:
# `copy` or `none` while it is over the window, then `dropped`, or
# `none: no drop` where letting go does nothing), and every file operation
# done. The Trash is a folder of the fixture: no test instance has another.
drop_project() {
    mkdir -p "$1/recipes/baking" "$1/trips"
    printf 'butter and flour\n' > "$1/recipes/baking/scones.md"
    printf 'flour, water, salt\n' > "$1/recipes/bread.md"
    printf 'leeks and potatoes\n' > "$1/recipes/soup.md"
    printf 'tram 28\n' > "$1/trips/lisbon.md"
    printf 'tomatoes in may\n' > "$1/garden.txt"
    printf 'a quiet day\n' > "$1/journal.md"
}
# drop_desk <dir>: what is dragged in, in a folder outside every project.
drop_desk() {
    mkdir -p "$1/letters/old"
    printf 'beans by the wall\n' > "$1/plots.txt"
    printf 'call the plumber\n' > "$1/notes.txt"
    printf 'pumpkin and sage\n' > "$1/soup.md"
    printf 'rye and caraway\n' > "$1/bread.md"
    printf 'dear anna\n' > "$1/letters/anna.md"
    printf 'a postcard\n' > "$1/letters/old/1999.md"
    ln -s anna.md "$1/letters/latest"
}
# drop_holds <file> <text>: the file is there with this one line in it.
drop_holds() {
    if [ "$(cat "$1" 2>/dev/null)" != "$2" ]; then
        failed "drop: $1 holds [$(cat "$1" 2>&1)], expected [$2]"
    fi
}
# drop_quiet <stderr file>: takes out the one line that is the system's and
# not crc's. Its pasteboard says it whenever a process that is not
# sandboxed reads a file's URL from it, and the read succeeds all the same.
drop_quiet() {
    grep -v '^sandbox_extension_consume failed: 1 (Operation not permitted)$' "$1" > "$1.kept" || true
    mv -f "$1.kept" "$1"
}
# drop_desk_whole <dir>: nothing dragged in was moved or changed.
drop_desk_whole() {
    drop_holds "$1/plots.txt" "beans by the wall"
    drop_holds "$1/notes.txt" "call the plumber"
    drop_holds "$1/soup.md" "pumpkin and sage"
    drop_holds "$1/bread.md" "rye and caraway"
    drop_holds "$1/letters/anna.md" "dear anna"
    drop_holds "$1/letters/old/1999.md" "a postcard"
}
D="$T/dropdesk"
drop_desk "$D"

# Over the editor a dragged file is still one to open, and the tree holds
# nothing. Over a folder's row the folder is where it would go, over a
# file's row the folder that file is in, and when the drag leaves the
# window nothing is held any more. Let go on a folder's row: one file is
# copied there, the folder opens, the copy is the selection and the tree
# has the keyboard. Several let go under the rows go to the project root, a
# folder with what is in it and a link in it as a link. A closed folder the
# drag rests on opens after a moment (a test instance is told a shorter
# one), and a file's row inside it is then a way into it.
drop_project "$T/dropproj"
cat > "$T/drop.script" <<SCRIPT
wait 500
dragfiles $D/plots.txt
dragover @text 0.5 0.5
dump $T/drop-over-editor.out
dragover @sidebar.row.0 0.5 0.5
dump $T/drop-over-folder.out
dragover @sidebar.row.2 0.5 0.5
dump $T/drop-over-file-row.out
dragout
dump $T/drop-left.out
dragover @sidebar.row.0 0.5 0.5
dropat @sidebar.row.0 0.5 0.5
wait 300
wait 300
dump $T/drop-one.out
dragfiles $D/notes.txt|$D/bread.md|$D/letters
dragover @sidebar 0.5 0.95
dump $T/drop-over-empty.out
dropat @sidebar 0.5 0.95
wait 300
wait 300
dump $T/drop-many.out
dragfiles $D/soup.md
dragover @sidebar.row.6 0.5 0.5
dump $T/drop-hover-start.out
wait 400
wait 200
dump $T/drop-hover-open.out
dragover @sidebar.row.7 0.5 0.5
dump $T/drop-hover-inside.out
dropat @sidebar.row.7 0.5 0.5
wait 300
wait 300
dump $T/drop-hovered.out
quit
SCRIPT
CRC_SELFTEST_HOVER_MS=250 CRC_SELFTEST_TRASH="$T/droptrash" CRC_SELFTEST="$T/drop.script" "$BIN" "$T/dropproj" 2> "$T/drop.err"
drop_quiet "$T/drop.err"
expect "$T/drop-over-editor.out" file_drag "copy"
expect "$T/drop-over-editor.out" tree_drag "none"
expect "$T/drop-over-folder.out" file_drag "copy"
expect "$T/drop-over-folder.out" tree_drag "from outside: plots.txt onto recipes"
expect "$T/drop-over-folder.out" file_ops ""
expect "$T/drop-over-file-row.out" tree_drag "from outside: plots.txt onto ."
expect "$T/drop-left.out" tree_drag "none"
expect "$T/drop-left.out" file_drag "left"
expect "$T/drop-one.out" file_drag "dropped"
expect "$T/drop-one.out" message "copied plots.txt to recipes"
expect "$T/drop-one.out" tree_drag "none"
expect "$T/drop-one.out" tree_keys true
expect "$T/drop-one.out" file_job "none"
expect "$T/drop-one.out" tree_rows "recipes/|recipes/baking/|recipes/bread.md|*>recipes/plots.txt|recipes/soup.md|trips/|garden.txt|journal.md"
expect "$T/drop-one.out" file_ops "copy $D/plots.txt > recipes/plots.txt"
# Dropped on the tree, a file is not opened.
expect "$T/drop-one.out" tabs "Home"
drop_holds "$T/dropproj/recipes/plots.txt" "beans by the wall"
expect "$T/drop-over-empty.out" tree_drag "from outside: 3 items onto ."
expect "$T/drop-over-empty.out" file_drag "copy"
expect "$T/drop-many.out" message "copied 3 items to dropproj"
expect "$T/drop-many.out" tree_keys true
expect "$T/drop-many.out" tree_rows "*>letters/|recipes/|recipes/baking/|recipes/bread.md|recipes/plots.txt|recipes/soup.md|trips/|*bread.md|garden.txt|journal.md|*notes.txt"
expect "$T/drop-many.out" file_ops "copy $D/plots.txt > recipes/plots.txt|copy $D/notes.txt > notes.txt|copy $D/bread.md > bread.md|copy $D/letters > letters"
drop_holds "$T/dropproj/notes.txt" "call the plumber"
drop_holds "$T/dropproj/bread.md" "rye and caraway"
drop_holds "$T/dropproj/letters/anna.md" "dear anna"
drop_holds "$T/dropproj/letters/old/1999.md" "a postcard"
if [ ! -L "$T/dropproj/letters/latest" ] || [ "$(readlink "$T/dropproj/letters/latest")" != anna.md ]; then
    failed "drop: the link in a dropped folder was not copied as a link"
fi
expect "$T/drop-hover-start.out" tree_drag "from outside: soup.md onto trips"
expect "$T/drop-hover-start.out" tree_rows "*>letters/|recipes/|recipes/baking/|recipes/bread.md|recipes/plots.txt|recipes/soup.md|trips/|*bread.md|garden.txt|journal.md|*notes.txt"
expect "$T/drop-hover-open.out" tree_drag "from outside: soup.md onto trips"
expect "$T/drop-hover-open.out" tree_rows "*>letters/|recipes/|recipes/baking/|recipes/bread.md|recipes/plots.txt|recipes/soup.md|trips/|trips/lisbon.md|*bread.md|garden.txt|journal.md|*notes.txt"
expect "$T/drop-hover-inside.out" tree_drag "from outside: soup.md onto trips"
expect "$T/drop-hovered.out" message "copied soup.md to trips"
expect "$T/drop-hovered.out" tree_rows "letters/|recipes/|recipes/baking/|recipes/bread.md|recipes/plots.txt|recipes/soup.md|trips/|trips/lisbon.md|*>trips/soup.md|bread.md|garden.txt|journal.md|notes.txt"
drop_holds "$T/dropproj/trips/soup.md" "pumpkin and sage"
# Copied, every time: nothing left the folder it was dragged from, and
# nothing went to the Trash.
drop_desk_whole "$D"
[ ! -e "$T/droptrash" ] || failed "drop: a drop with no name in use sent something to the Trash"
if [ -n "$(find "$T/dropproj" -name '.crc-copy-*' 2>/dev/null)" ]; then
    failed "drop: a copy left the folder it was built in behind"
fi

# Names in use. One question for the whole drop, answered here in order by
# the test instance: Replace, Keep Both, Cancel, and then no answer, where
# it says what it would ask. Replace moves each item that is there to the
# Trash and the copy takes its name: the old text is in the Trash, where
# the operation says, and the new one in its place, in the tab that was
# open on the file too. Keep Both gives every
# copy the next free name. Cancel copies nothing at all, the file whose
# name was free included.
drop_project "$T/dropproj2"
cat > "$T/drop-taken.script" <<SCRIPT
wait 500
key 35 cmd p
text soup.md
key 36
wait 300
dragfiles $D/soup.md|$D/bread.md|$D/plots.txt
dragover @sidebar.row.0 0.5 0.5
dump $T/drop-taken-over.out
dropat @sidebar.row.0 0.5 0.5
wait 300
wait 300
dump $T/drop-replace.out
dragover @sidebar.row.2 0.5 0.5
dropat @sidebar.row.2 0.5 0.5
wait 300
wait 300
dump $T/drop-keep-both.out
dragover @sidebar.row.2 0.5 0.5
dropat @sidebar.row.2 0.5 0.5
wait 300
wait 300
dump $T/drop-cancel.out
dragfiles $D/soup.md
dragover @sidebar.row.2 0.5 0.5
dropat @sidebar.row.2 0.5 0.5
wait 300
dump $T/drop-ask-one.out
dragfiles $D/soup.md|$D/bread.md|$D/notes.txt
dragover @sidebar.row.2 0.5 0.5
dropat @sidebar.row.2 0.5 0.5
wait 300
dump $T/drop-ask-many.out
quit
SCRIPT
CRC_DROP_ANSWER="replace,keep-both,cancel" CRC_SELFTEST_TRASH="$T/droptrash2" CRC_SELFTEST="$T/drop-taken.script" "$BIN" "$T/dropproj2" 2> "$T/drop-taken.err"
drop_quiet "$T/drop-taken.err"
# A name in use is no reason not to drop: the question comes after.
expect "$T/drop-taken-over.out" file_drag "copy"
expect "$T/drop-taken-over.out" tree_drag "from outside: 3 items onto recipes"
expect "$T/drop-replace.out" message "copied 3 items to recipes; the 2 items replaced are in the Trash"
expect "$T/drop-replace.out" tree_keys true
expect "$T/drop-replace.out" tree_rows "recipes/|recipes/baking/|*>recipes/bread.md|*recipes/plots.txt|*recipes/soup.md|trips/|garden.txt|journal.md"
expect "$T/drop-replace.out" file_ops "replace $D/soup.md > recipes/soup.md trashed $T/droptrash2/soup.md|replace $D/bread.md > recipes/bread.md trashed $T/droptrash2/bread.md|copy $D/plots.txt > recipes/plots.txt"
drop_holds "$T/dropproj2/recipes/soup.md" "pumpkin and sage"
drop_holds "$T/dropproj2/recipes/bread.md" "rye and caraway"
drop_holds "$T/dropproj2/recipes/plots.txt" "beans by the wall"
drop_holds "$T/droptrash2/soup.md" "leeks and potatoes"
drop_holds "$T/droptrash2/bread.md" "flour, water, salt"
# The tab that was open on the file replaced, with nothing unsaved in it,
# shows what is there now.
expect "$T/drop-replace.out" tabs "soup.md"
expect_line "$T/drop-replace.out" 1 "pumpkin and sage"
expect "$T/drop-replace.out" dirty false
[ "$(ls "$T/droptrash2" | wc -l | tr -d ' ')" = 2 ] || failed "drop: Replace sent [$(ls "$T/droptrash2" | tr '\n' ' ')] to the Trash, expected the two items replaced"
expect "$T/drop-keep-both.out" message "copied 3 items to recipes"
expect "$T/drop-keep-both.out" tree_rows "recipes/|recipes/baking/|*>recipes/bread-copy.md|recipes/bread.md|*recipes/plots-copy.txt|recipes/plots.txt|*recipes/soup-copy.md|recipes/soup.md|trips/|garden.txt|journal.md"
expect "$T/drop-keep-both.out" file_ops "replace $D/soup.md > recipes/soup.md trashed $T/droptrash2/soup.md|replace $D/bread.md > recipes/bread.md trashed $T/droptrash2/bread.md|copy $D/plots.txt > recipes/plots.txt|copy $D/soup.md > recipes/soup-copy.md|copy $D/bread.md > recipes/bread-copy.md|copy $D/plots.txt > recipes/plots-copy.txt"
drop_holds "$T/dropproj2/recipes/soup-copy.md" "pumpkin and sage"
drop_holds "$T/dropproj2/recipes/soup.md" "pumpkin and sage"
# Cancel: the dump is the one before it, word for word where it matters.
expect "$T/drop-cancel.out" file_drag "dropped"
expect "$T/drop-cancel.out" message "copied 3 items to recipes"
expect "$T/drop-cancel.out" file_ops "replace $D/soup.md > recipes/soup.md trashed $T/droptrash2/soup.md|replace $D/bread.md > recipes/bread.md trashed $T/droptrash2/bread.md|copy $D/plots.txt > recipes/plots.txt|copy $D/soup.md > recipes/soup-copy.md|copy $D/bread.md > recipes/bread-copy.md|copy $D/plots.txt > recipes/plots-copy.txt"
[ "$(ls "$T/dropproj2/recipes" | wc -l | tr -d ' ')" = 7 ] || failed "drop: a cancelled drop changed the folder: [$(ls "$T/dropproj2/recipes" | tr '\n' ' ')]"
[ "$(ls "$T/droptrash2" | wc -l | tr -d ' ')" = 2 ] || failed "drop: a cancelled or kept drop sent something to the Trash"
# The question: the one item by its name, several by how many of them
# have their name in use, which is not all that was dropped.
expect "$T/drop-ask-one.out" message "would ask: “soup.md” already exists in “recipes”."
expect "$T/drop-ask-many.out" message "would ask: 2 of these items already exist in “recipes”."
[ ! -e "$T/dropproj2/recipes/notes.txt" ] || failed "drop: a drop whose question was not answered copied a file"
drop_desk_whole "$D"

# What does nothing, and what is refused. A file dropped on the folder it
# is in already: no folder lights up, the drag is answered none, and
# letting go asks nothing and does nothing. A folder over a folder inside
# itself likewise, and beside a file that can go it is said to be refused
# while the file is copied. A file that is not there when it is dropped is
# skipped and said so. Text is not files: over the tree it is answered
# none and dropped there it is ignored. A file let go over the editor is
# opened, as before, and copied nowhere. With unsaved work in a tab on a
# file, Replace is refused before anything goes to the Trash.
drop_project "$T/dropproj3"
cat > "$T/drop-refuse.script" <<SCRIPT
wait 500
dragfiles $T/dropproj3/garden.txt
dragover @sidebar 0.5 0.95
dump $T/drop-own-over.out
dropat @sidebar 0.5 0.95
wait 300
dump $T/drop-own.out
clickat @sidebar.row.0 0.5 0.5
wait 200
dragfiles $T/dropproj3/recipes
dragover @sidebar.row.1 0.5 0.5
dump $T/drop-itself-over.out
dropat @sidebar.row.1 0.5 0.5
wait 300
dump $T/drop-itself.out
dragfiles $T/dropproj3/recipes|$D/notes.txt
dragover @sidebar.row.1 0.5 0.5
dump $T/drop-itself-mixed-over.out
dropat @sidebar.row.1 0.5 0.5
wait 300
wait 300
dump $T/drop-itself-mixed.out
dragfiles $D/gone.txt|$D/plots.txt
dragover @sidebar.row.6 0.5 0.5
dump $T/drop-gone-over.out
dropat @sidebar.row.6 0.5 0.5
wait 300
wait 300
dump $T/drop-gone.out
dragtext a line of text
dragover @sidebar.row.0 0.5 0.5
dump $T/drop-text-over.out
dropat @sidebar.row.0 0.5 0.5
wait 200
dump $T/drop-text.out
dragover @text 0.5 0.5
dump $T/drop-text-editor.out
dragfiles $D/plots.txt
dragover @text 0.5 0.5
dropat @text 0.5 0.5
wait 300
dump $T/drop-editor.out
key 35 cmd p
text soup.md
key 36
wait 300
text x
dragfiles $D/soup.md
dragover @sidebar.row.0 0.5 0.5
dropat @sidebar.row.0 0.5 0.5
wait 300
wait 300
dump $T/drop-unsaved.out
quit
SCRIPT
CRC_DROP_ANSWER="replace" CRC_SELFTEST_TRASH="$T/droptrash3" CRC_SELFTEST="$T/drop-refuse.script" "$BIN" "$T/dropproj3" 2> "$T/drop-refuse.err"
drop_quiet "$T/drop-refuse.err"
expect "$T/drop-own-over.out" file_drag "none"
expect "$T/drop-own-over.out" tree_drag "from outside: garden.txt onto nowhere"
expect "$T/drop-own.out" file_drag "none: no drop"
expect "$T/drop-own.out" tree_drag "none"
expect "$T/drop-own.out" message "Folder opened"
expect "$T/drop-own.out" file_ops ""
expect "$T/drop-own.out" tree_rows "recipes/|trips/|garden.txt|journal.md"
expect "$T/drop-itself-over.out" file_drag "none"
expect "$T/drop-itself-over.out" tree_drag "from outside: recipes onto nowhere"
expect "$T/drop-itself.out" file_drag "none: no drop"
expect "$T/drop-itself.out" file_ops ""
expect "$T/drop-itself-mixed-over.out" file_drag "copy"
expect "$T/drop-itself-mixed-over.out" tree_drag "from outside: 2 items onto recipes/baking"
expect "$T/drop-itself-mixed.out" message "copied notes.txt to baking; recipes cannot be copied into itself"
expect "$T/drop-itself-mixed.out" file_ops "copy $D/notes.txt > recipes/baking/notes.txt"
[ ! -e "$T/dropproj3/recipes/baking/recipes" ] || failed "drop: a folder dropped into itself was copied there"
expect "$T/drop-gone-over.out" file_drag "copy"
expect "$T/drop-gone.out" message "copied plots.txt to trips; gone.txt is no longer there"
expect "$T/drop-gone.out" file_ops "copy $D/notes.txt > recipes/baking/notes.txt|copy $D/plots.txt > trips/plots.txt"
expect "$T/drop-text-over.out" file_drag "none"
expect "$T/drop-text-over.out" tree_drag "none"
expect "$T/drop-text.out" file_drag "none: no drop"
expect "$T/drop-text.out" file_ops "copy $D/notes.txt > recipes/baking/notes.txt|copy $D/plots.txt > trips/plots.txt"
expect "$T/drop-text-editor.out" file_drag "none"
# On the editor: opened in a tab, from where it is, and copied nowhere.
expect "$T/drop-editor.out" file_drag "dropped"
expect "$T/drop-editor.out" tabs "plots.txt"
expect_line "$T/drop-editor.out" 1 "beans by the wall"
expect "$T/drop-editor.out" file_ops "copy $D/notes.txt > recipes/baking/notes.txt|copy $D/plots.txt > trips/plots.txt"
[ ! -e "$T/dropproj3/plots.txt" ] || failed "drop: a file dropped on the editor was copied into the project"
expect "$T/drop-unsaved.out" message "save the affected tabs before replacing this item"
expect "$T/drop-unsaved.out" file_ops "copy $D/notes.txt > recipes/baking/notes.txt|copy $D/plots.txt > trips/plots.txt"
drop_holds "$T/dropproj3/recipes/soup.md" "leeks and potatoes"
[ ! -e "$T/droptrash3" ] || failed "drop: a refused Replace sent something to the Trash"
drop_desk_whole "$D"
