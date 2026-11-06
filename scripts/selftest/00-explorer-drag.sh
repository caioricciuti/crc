# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh explorer-drag.

# ---- dragging in the Explorer ----------------------------------------------
# A drag that starts on one of the selected rows carries all of them, and
# on any other row that row alone. Let go over a folder's row it goes into
# that folder, over a file's row into the folder the file is in, and under
# the rows into the project root. With Option held when it is let go it
# copies. Every dump says what is held (`tree_drag:`: what the pill beside
# the pointer says, `copy` while Option is down, and the folder it would
# drop into: `.` for the root, `nowhere` where nothing would go), lists the
# rows by their path in the project (a selected one starred, the cursor's
# after a `>`), and every file operation done. Nothing is ever replaced.
drag_project() {
    mkdir -p "$1/recipes/baking" "$1/trips"
    printf 'butter and flour\n' > "$1/recipes/baking/scones.md"
    printf 'flour, water, salt\n' > "$1/recipes/bread.md"
    printf 'leeks and potatoes\n' > "$1/recipes/soup.md"
    printf 'tram 28\n' > "$1/trips/lisbon.md"
    printf 'a bowl in alfama\n' > "$1/trips/soup.md"
    printf 'tomatoes in may\n' > "$1/garden.txt"
    printf 'oat milk\n' > "$1/groceries.txt"
    printf 'a quiet day\n' > "$1/journal.md"
    printf 'dear anna\n' > "$1/letters.txt"
}
# drag_kept <path under the run's folder>: reason:size of each version
# local history keeps for it, oldest first.
drag_kept() {
    /usr/bin/sqlite3 "$CRC_HISTORY_DB" "SELECT group_concat(row, ' ') FROM (
        SELECT s.reason || ':' || b.size AS row FROM snapshots s
        JOIN files f ON f.id = s.file JOIN blobs b ON b.id = s.blob
        WHERE f.path LIKE '%/$1' ORDER BY s.id)" 2>&1
}
# drag_holds <file> <text>: the file is there with this one line in it.
drag_holds() {
    if [ "$(cat "$1" 2>/dev/null)" != "$2" ]; then
        failed "drag: $1 holds [$(cat "$1" 2>&1)], expected [$2]"
    fi
}

# Several at once, with their tabs and their local history. Two of the
# three are open in tabs, edited and saved, so each has versions kept.
# Pressed on one of the three selected rows and dragged to a folder, all
# three go, the folder opens, and they are the selection. What was kept
# for each is under its new path afterwards and nothing is left under the
# old one. Then one row alone: onto a file's row it goes into that file's
# folder, under the rows into the project root, and onto the folder it is
# already in nowhere, with nothing said and nothing done. A file that is
# picked up and dragged is not opened: only a press let go where it went
# down opens one.
drag_project "$T/dragproj"
cat > "$T/drag.script" <<SCRIPT
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
dump $T/drag-pressed.out
dragat @sidebar.row.0 0.5 0.5
dump $T/drag-held.out
upat @sidebar.row.0 0.5 0.5
wait 300
wait 300
dump $T/drag-many.out
downat @sidebar.row.8 0.5 0.5
dragat @sidebar.row.2 0.5 0.5
dump $T/drag-file-row-held.out
upat @sidebar.row.2 0.5 0.5
wait 300
wait 300
dump $T/drag-file-row.out
downat @sidebar.row.4 0.5 0.5
drag 150 650
dump $T/drag-root-held.out
up 150 650
wait 300
wait 300
dump $T/drag-root.out
downat @sidebar.row.2 0.5 0.5
dragat @sidebar.row.0 0.5 0.5
dump $T/drag-stays-held.out
upat @sidebar.row.0 0.5 0.5
wait 300
dump $T/drag-stays.out
quit
SCRIPT
CRC_SELFTEST="$T/drag.script" "$BIN" "$T/dragproj" 2> "$T/drag.err"
# A press alone is no drag yet, and the three stay selected under it.
expect "$T/drag-pressed.out" tree_drag "none"
expect "$T/drag-pressed.out" tree_rows "recipes/|trips/|*garden.txt|*groceries.txt|*>journal.md|letters.txt"
expect "$T/drag-held.out" tree_drag "3 items onto recipes"
expect "$T/drag-held.out" file_ops ""
expect "$T/drag-many.out" message "moved 3 items to recipes"
expect "$T/drag-many.out" tree_drag "none"
expect "$T/drag-many.out" tree_keys true
expect "$T/drag-many.out" tree_rows "recipes/|recipes/baking/|recipes/bread.md|*>recipes/garden.txt|*recipes/groceries.txt|*recipes/journal.md|recipes/soup.md|trips/|letters.txt"
expect "$T/drag-many.out" file_ops "move garden.txt > recipes/garden.txt|move groceries.txt > recipes/groceries.txt|move journal.md > recipes/journal.md"
# The tabs went with their files: still two, on the names they had.
expect "$T/drag-many.out" tabs "journal.md | garden.txt"
drag_holds "$T/dragproj/recipes/journal.md" "aa quiet day"
drag_holds "$T/dragproj/recipes/garden.txt" "btomatoes in may"
for drag_gone in garden.txt journal.md letters.txt; do
    [ ! -e "$T/dragproj/$drag_gone" ] || failed "drag: $drag_gone is still in the project root"
done
[ "$(drag_kept dragproj/recipes/journal.md)" = "1:12 2:13" ] \
    || failed "drag: history of journal.md at its new path is [$(drag_kept dragproj/recipes/journal.md)], expected [1:12 2:13]"
[ "$(drag_kept dragproj/recipes/garden.txt)" = "1:16 2:17" ] \
    || failed "drag: history of garden.txt at its new path is [$(drag_kept dragproj/recipes/garden.txt)], expected [1:16 2:17]"
[ -z "$(drag_kept dragproj/journal.md)" ] \
    || failed "drag: history was left at journal.md's old path: [$(drag_kept dragproj/journal.md)]"
[ -z "$(drag_kept dragproj/garden.txt)" ] \
    || failed "drag: history was left at garden.txt's old path: [$(drag_kept dragproj/garden.txt)]"
# Onto the row of a file in a folder: into that folder.
expect "$T/drag-file-row-held.out" tree_drag "letters.txt onto recipes"
expect "$T/drag-file-row.out" message "moved letters.txt to recipes"
# letters.txt was not open, and dragging it did not open it.
expect "$T/drag-file-row-held.out" tabs "journal.md | garden.txt"
expect "$T/drag-file-row.out" tabs "journal.md | garden.txt"
expect "$T/drag-file-row.out" tree_rows "recipes/|recipes/baking/|recipes/bread.md|recipes/garden.txt|recipes/groceries.txt|recipes/journal.md|*>recipes/letters.txt|recipes/soup.md|trips/"
drag_holds "$T/dragproj/recipes/letters.txt" "dear anna"
# Under the rows: the project root.
expect "$T/drag-root-held.out" tree_drag "groceries.txt onto ."
expect "$T/drag-root.out" message "moved groceries.txt to dragproj"
expect "$T/drag-root.out" tree_rows "recipes/|recipes/baking/|recipes/bread.md|recipes/garden.txt|recipes/journal.md|recipes/letters.txt|recipes/soup.md|trips/|*>groceries.txt"
drag_holds "$T/dragproj/groceries.txt" "oat milk"
[ ! -e "$T/dragproj/recipes/groceries.txt" ] || failed "drag: groceries.txt is still in recipes"
# Onto the folder it is in already: no target lights up, and letting go
# does and says nothing, and opens nothing either.
expect "$T/drag-stays-held.out" tree_drag "bread.md onto nowhere"
expect "$T/drag-stays.out" message "moved groceries.txt to dragproj"
expect "$T/drag-stays.out" tabs "journal.md | garden.txt"
expect "$T/drag-stays.out" file_ops "move garden.txt > recipes/garden.txt|move groceries.txt > recipes/groceries.txt|move journal.md > recipes/journal.md|move letters.txt > recipes/letters.txt|move recipes/groceries.txt > groceries.txt"
drag_holds "$T/dragproj/recipes/bread.md" "flour, water, salt"

# What is refused. A folder is opened with a click, and then pressed and
# dragged: the press leaves it open, as it is only a click let go that
# opens or closes one. Dragged onto a folder inside it, it lights nothing
# up and, let go, is said to be refused. Two files dragged to a folder
# where one of their names is in use: the other goes, the one is skipped
# and said so, and neither file of that name is touched. Escape while the
# drag is held calls it off: the release after it moves nothing.
drag_project "$T/dragproj2"
cat > "$T/drag-refuse.script" <<SCRIPT
wait 500
clickat @sidebar.row.0 0.5 0.5
wait 200
downat @sidebar.row.0 0.5 0.5
dump $T/drag-itself-pressed.out
dragat @sidebar.row.1 0.5 0.5
dump $T/drag-itself-held.out
upat @sidebar.row.1 0.5 0.5
wait 200
dump $T/drag-itself.out
click 150 650
clickat @sidebar.row.2 0.5 0.5 cmd
clickat @sidebar.row.3 0.5 0.5 cmd
downat @sidebar.row.2 0.5 0.5
dragat @sidebar.row.4 0.5 0.5
dump $T/drag-taken-held.out
upat @sidebar.row.4 0.5 0.5
wait 300
wait 300
dump $T/drag-taken.out
downat @sidebar.row.7 0.5 0.5
dragat @sidebar.row.0 0.5 0.5
dump $T/drag-escape-held.out
key 53
dump $T/drag-escape.out
upat @sidebar.row.0 0.5 0.5
wait 300
dump $T/drag-escaped.out
quit
SCRIPT
CRC_SELFTEST="$T/drag-refuse.script" "$BIN" "$T/dragproj2" 2> "$T/drag-refuse.err"
expect "$T/drag-itself-pressed.out" tree_rows "*>recipes/|recipes/baking/|recipes/bread.md|recipes/soup.md|trips/|garden.txt|groceries.txt|journal.md|letters.txt"
expect "$T/drag-itself-held.out" tree_rows "*>recipes/|recipes/baking/|recipes/bread.md|recipes/soup.md|trips/|garden.txt|groceries.txt|journal.md|letters.txt"
expect "$T/drag-itself-held.out" tree_drag "recipes onto nowhere"
expect "$T/drag-itself.out" message "recipes cannot be moved into itself"
expect "$T/drag-itself.out" file_ops ""
expect "$T/drag-itself.out" tree_rows "*>recipes/|recipes/baking/|recipes/bread.md|recipes/soup.md|trips/|garden.txt|groceries.txt|journal.md|letters.txt"
[ -d "$T/dragproj2/recipes/baking" ] && [ ! -e "$T/dragproj2/recipes/baking/recipes" ] \
    || failed "drag: a folder dropped into itself did not stay where it was"
expect "$T/drag-taken-held.out" tree_drag "2 items onto trips"
expect "$T/drag-taken.out" message "moved bread.md to trips; skipped soup.md: an item of that name is already there"
expect "$T/drag-taken.out" tree_rows "recipes/|recipes/baking/|recipes/soup.md|trips/|*>trips/bread.md|trips/lisbon.md|trips/soup.md|garden.txt|groceries.txt|journal.md|letters.txt"
expect "$T/drag-taken.out" file_ops "move recipes/bread.md > trips/bread.md"
drag_holds "$T/dragproj2/trips/bread.md" "flour, water, salt"
drag_holds "$T/dragproj2/recipes/soup.md" "leeks and potatoes"
drag_holds "$T/dragproj2/trips/soup.md" "a bowl in alfama"
expect "$T/drag-escape-held.out" tree_drag "garden.txt onto recipes"
expect "$T/drag-escape.out" tree_drag "none"
expect "$T/drag-escaped.out" tree_drag "none"
expect "$T/drag-escaped.out" file_ops "move recipes/bread.md > trips/bread.md"
# Nothing was opened by any of it: no press here was let go as a click on
# a file.
expect "$T/drag-escaped.out" tabs "Home"
drag_holds "$T/dragproj2/garden.txt" "tomatoes in may"
[ ! -e "$T/dragproj2/recipes/garden.txt" ] || failed "drag: a drag called off with Escape moved its file"

# A closed folder the drag rests on opens, after a moment and not at once
# (a test instance is told a shorter moment), and a file's row inside it is
# then a way into it. Option makes the drop a copy, read as the key goes
# down and up with the pointer still: over the folder the file is in, a
# move has nowhere to go and a copy does. Let go with Option held the file
# is copied beside itself as -copy, and again as -copy-2; a folder is
# copied with what is in it, and the original stays.
drag_project "$T/dragproj3"
cat > "$T/drag-copy.script" <<SCRIPT
wait 500
downat @sidebar.row.5 0.5 0.5
dragat @sidebar.row.1 0.5 0.5
dump $T/drag-hover-start.out
wait 400
wait 200
dump $T/drag-hover-open.out
dragat @sidebar.row.2 0.5 0.5
dump $T/drag-hover-inside.out
key 53
upat @sidebar.row.2 0.5 0.5
downat @sidebar.row.7 0.5 0.5
dragat @sidebar.row.4 0.5 0.5
dump $T/drag-copy-plain.out
flags opt
dump $T/drag-copy-opt.out
flags -
dump $T/drag-copy-let-go.out
upat @sidebar.row.4 0.5 0.5 opt
wait 300
wait 300
dump $T/drag-copy.out
downat @sidebar.row.8 0.5 0.5
dragat @sidebar.row.4 0.5 0.5 opt
upat @sidebar.row.4 0.5 0.5 opt
wait 300
wait 300
dump $T/drag-copy-again.out
click 150 650
downat @sidebar.row.0 0.5 0.5
dragat @sidebar.row.1 0.5 0.5 opt
dump $T/drag-copy-folder-held.out
upat @sidebar.row.1 0.5 0.5 opt
wait 300
wait 300
dump $T/drag-copy-folder.out
quit
SCRIPT
CRC_SELFTEST_HOVER_MS=250 CRC_SELFTEST="$T/drag-copy.script" "$BIN" "$T/dragproj3" 2> "$T/drag-copy.err"
expect "$T/drag-hover-start.out" tree_drag "letters.txt onto trips"
expect "$T/drag-hover-start.out" tree_rows "recipes/|trips/|garden.txt|groceries.txt|journal.md|*>letters.txt"
expect "$T/drag-hover-open.out" tree_drag "letters.txt onto trips"
expect "$T/drag-hover-open.out" tree_rows "recipes/|trips/|trips/lisbon.md|trips/soup.md|garden.txt|groceries.txt|journal.md|*>letters.txt"
expect "$T/drag-hover-inside.out" tree_drag "letters.txt onto trips"
drag_holds "$T/dragproj3/letters.txt" "dear anna"
expect "$T/drag-copy-plain.out" tree_drag "letters.txt onto nowhere"
expect "$T/drag-copy-opt.out" tree_drag "letters.txt copy onto ."
expect "$T/drag-copy-let-go.out" tree_drag "letters.txt onto nowhere"
expect "$T/drag-copy.out" message "copied letters-copy.txt to dragproj3"
expect "$T/drag-copy.out" file_job "none"
expect "$T/drag-copy.out" tree_rows "recipes/|trips/|trips/lisbon.md|trips/soup.md|garden.txt|groceries.txt|journal.md|*>letters-copy.txt|letters.txt"
expect "$T/drag-copy.out" file_ops "copy letters.txt > letters-copy.txt"
expect "$T/drag-copy-again.out" message "copied letters-copy-2.txt to dragproj3"
expect "$T/drag-copy-again.out" file_ops "copy letters.txt > letters-copy.txt|copy letters.txt > letters-copy-2.txt"
drag_holds "$T/dragproj3/letters.txt" "dear anna"
drag_holds "$T/dragproj3/letters-copy.txt" "dear anna"
drag_holds "$T/dragproj3/letters-copy-2.txt" "dear anna"
expect "$T/drag-copy-folder-held.out" tree_drag "recipes copy onto trips"
expect "$T/drag-copy-folder.out" message "copied recipes to trips"
expect "$T/drag-copy-folder.out" file_ops "copy letters.txt > letters-copy.txt|copy letters.txt > letters-copy-2.txt|copy recipes > trips/recipes"
drag_holds "$T/dragproj3/trips/recipes/baking/scones.md" "butter and flour"
drag_holds "$T/dragproj3/recipes/baking/scones.md" "butter and flour"
if [ -n "$(find "$T/dragproj3" -name '.crc-copy-*' 2>/dev/null)" ]; then
    failed "drag: a copy left the folder it was built in behind"
fi

# Held near the bottom of the rows the tree scrolls, by the frame and with
# the pointer still, and near the top it scrolls back.
mkdir -p "$T/dragproj4"
drag_n=1
while [ "$drag_n" -le 60 ]; do
    printf 'day %s\n' "$drag_n" > "$T/dragproj4/$(printf 'day-%02d.txt' "$drag_n")"
    drag_n=$((drag_n + 1))
done
cat > "$T/drag-scroll.script" <<SCRIPT
resize 900 500
wait 500
downat @sidebar.row.1 0.5 0.5
dragat @sidebar.row.2 0.5 0.5
dump $T/drag-scroll-start.out
drag 150 470
wait 400
dump $T/drag-scroll-down.out
drag 150 95
wait 600
wait 600
wait 600
dump $T/drag-scroll-up.out
key 53
up 150 95
quit
SCRIPT
CRC_SELFTEST="$T/drag-scroll.script" "$BIN" "$T/dragproj4" 2> "$T/drag-scroll.err"
expect "$T/drag-scroll-start.out" tree_scroll 0
drag_down=$(grep -m1 '^tree_scroll: ' "$T/drag-scroll-down.out" 2>/dev/null | sed 's/^tree_scroll: //' || true)
if ! [ "${drag_down:-0}" -ge 2 ] 2>/dev/null; then
    failed "drag: held at the bottom of the rows the tree scrolled to [$drag_down], expected 2 rows or more"
fi
expect "$T/drag-scroll-up.out" tree_scroll 0
[ "$(ls "$T/dragproj4" | wc -l | tr -d ' ')" = 60 ] || failed "drag: a drag called off while scrolling changed the project"
