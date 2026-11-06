# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh explorer-clipboard.

# ---- copy, cut, paste and duplicate in the Explorer -------------------------
# While the tree has the keyboard, Cmd-C, Cmd-X and Cmd-V act on the
# selected files and folders, through the clipboard: a test instance has a
# private one, so nothing here reads or replaces what the person running it
# has copied. Every dump lists the tree's rows by their path in the project
# (a cut row after a `~`, a selected one starred, the cursor's after a `>`),
# what is on the clipboard, the copy running, and every file operation done.
# Nothing is ever replaced, nothing goes to the Trash, and the folder a copy
# is built in never shows as a row.
#
# A click under the rows gives the tree the keyboard with nothing selected;
# a Command-click then selects a row without opening it.
clip_project() {
    mkdir -p "$1/recipes" "$1/trips"
    printf 'flour, water, salt\n' > "$1/recipes/bread.md"
    printf 'leeks and potatoes\n' > "$1/recipes/soup.md"
    printf 'tram 28\n' > "$1/trips/lisbon.md"
    printf 'tomatoes in may\n' > "$1/garden.txt"
    printf 'oat milk\n' > "$1/groceries.txt"
    printf 'a quiet day\n' > "$1/journal.md"
    printf 'dear anna\n' > "$1/letters.txt"
}
# clip_staging <dir>: the hidden folders copies are built in, anywhere
# under <dir>, one to a line.
clip_staging() {
    find "$1" -name '.crc-copy-*' 2>/dev/null || true
}
# clip_quiet <stderr file>: takes out the one line that is the system's and
# not crc's. Its pasteboard says it whenever a process that is not
# sandboxed reads a file's URL from it, and the read succeeds all the same.
clip_quiet() {
    grep -v '^sandbox_extension_consume failed: 1 (Operation not permitted)$' "$1" > "$1.kept" || true
    mv -f "$1.kept" "$1"
}
# clip_no_staging <dump>: no row of the dumped tree is such a folder.
clip_no_staging() {
    if grep -m1 '^tree_rows: ' "$1" 2>/dev/null | grep -qF '.crc-copy-'; then
        failed "$(basename "$1"): a folder a copy is built in shows as a row: $(grep -m1 '^tree_rows: ' "$1")"
    fi
}

# Copy and paste. With no files on the clipboard a paste says so. A file
# pasted into the folder it came from keeps both: `-copy`, then `-copy-2`.
# A folder copied and pasted at once, the cursor still on it, lands beside
# itself the same way, and pasted again straight after, the cursor now on
# that copy, beside it again: never one copy inside the other. Pasted on a
# folder's row chosen on purpose, a copy just made included, it goes into
# that folder with what is in it, and a folder inside itself is refused.
# Several items go together. What was pasted is selected, in a folder that
# was closed too.
clip_project "$T/clipproj"
cat > "$T/clip.script" <<SCRIPT
wait 500
click 150 650
key 9 cmd v
dump $T/clip-empty.out
clickat @sidebar.row.2 0.5 0.5 cmd
key 8 cmd c
dump $T/clip-copied.out
key 9 cmd v
wait 300
wait 300
dump $T/clip-paste.out
key 9 cmd v
wait 300
wait 300
dump $T/clip-paste-again.out
click 150 650
clickat @sidebar.row.0 0.5 0.5 cmd
key 8 cmd c
key 9 cmd v
wait 300
wait 300
dump $T/clip-beside.out
key 9 cmd v
wait 300
wait 300
dump $T/clip-beside-again.out
click 150 650
clickat @sidebar.row.2 0.5 0.5 cmd
key 9 cmd v
wait 300
wait 300
dump $T/clip-into-copy.out
click 150 650
clickat @sidebar.row.6 0.5 0.5 cmd
key 9 cmd v
wait 300
wait 300
dump $T/clip-folder.out
click 150 650
clickat @sidebar.row.6 0.5 0.5 cmd
key 8 cmd c
click 150 650
clickat @sidebar.row.7 0.5 0.5 cmd
key 9 cmd v
wait 300
dump $T/clip-itself.out
click 150 650
clickat @sidebar.row.12 0.5 0.5 cmd
clickat @sidebar.row.13 0.5 0.5 cmd
key 8 cmd c
dump $T/clip-many-copied.out
click 150 650
clickat @sidebar.row.0 0.5 0.5 cmd
key 9 cmd v
wait 300
wait 300
dump $T/clip-many.out
quit
SCRIPT
CRC_SELFTEST="$T/clip.script" "$BIN" "$T/clipproj" 2> "$T/clip.err"
clip_quiet "$T/clip.err"
expect "$T/clip-empty.out" tree_keys true
expect "$T/clip-empty.out" message "nothing to paste: the clipboard holds no files"
expect "$T/clip-empty.out" clipboard_files "none"
expect "$T/clip-copied.out" message "copied garden.txt"
expect "$T/clip-copied.out" clipboard_files "copy garden.txt"
expect "$T/clip-copied.out" clipboard_text "garden.txt"
expect "$T/clip-copied.out" tree_rows "recipes/|trips/|*>garden.txt|groceries.txt|journal.md|letters.txt"
expect "$T/clip-paste.out" message "pasted garden-copy.txt"
expect "$T/clip-paste.out" tree_rows "recipes/|trips/|*>garden-copy.txt|garden.txt|groceries.txt|journal.md|letters.txt"
expect "$T/clip-paste.out" file_job "none"
expect "$T/clip-paste-again.out" message "pasted garden-copy-2.txt"
expect "$T/clip-paste-again.out" tree_rows "recipes/|trips/|*>garden-copy-2.txt|garden-copy.txt|garden.txt|groceries.txt|journal.md|letters.txt"
# Nothing was opened by any of it.
expect "$T/clip-paste-again.out" tabs "Home"
# Select a folder, copy, paste, paste: beside it both times, not into it
# and not into the first copy, which has the cursor for the second paste.
expect "$T/clip-beside.out" message "pasted recipes-copy"
expect "$T/clip-beside.out" tree_rows "recipes/|*>recipes-copy/|trips/|garden-copy-2.txt|garden-copy.txt|garden.txt|groceries.txt|journal.md|letters.txt"
expect "$T/clip-beside-again.out" message "pasted recipes-copy-2"
expect "$T/clip-beside-again.out" tree_rows "recipes/|recipes-copy/|*>recipes-copy-2/|trips/|garden-copy-2.txt|garden-copy.txt|garden.txt|groceries.txt|journal.md|letters.txt"
# The cursor put on the second copy on purpose: into it.
expect "$T/clip-into-copy.out" message "pasted recipes"
expect "$T/clip-into-copy.out" tree_rows "recipes/|recipes-copy/|recipes-copy-2/|*>recipes-copy-2/recipes/|recipes-copy-2/bread.md|recipes-copy-2/soup.md|trips/|garden-copy-2.txt|garden-copy.txt|garden.txt|groceries.txt|journal.md|letters.txt"
expect "$T/clip-folder.out" message "pasted recipes"
expect "$T/clip-folder.out" tree_rows "recipes/|recipes-copy/|recipes-copy-2/|recipes-copy-2/recipes/|recipes-copy-2/bread.md|recipes-copy-2/soup.md|trips/|*>trips/recipes/|trips/lisbon.md|garden-copy-2.txt|garden-copy.txt|garden.txt|groceries.txt|journal.md|letters.txt"
# trips, pasted on a folder that is in it.
expect "$T/clip-itself.out" message "trips cannot be pasted into itself"
expect "$T/clip-itself.out" tree_rows "recipes/|recipes-copy/|recipes-copy-2/|recipes-copy-2/recipes/|recipes-copy-2/bread.md|recipes-copy-2/soup.md|trips/|*>trips/recipes/|trips/lisbon.md|garden-copy-2.txt|garden-copy.txt|garden.txt|groceries.txt|journal.md|letters.txt"
expect "$T/clip-many-copied.out" message "copied 2 items"
expect "$T/clip-many-copied.out" clipboard_files "copy groceries.txt|journal.md"
expect "$T/clip-many-copied.out" clipboard_text "groceries.txt|journal.md"
expect "$T/clip-many.out" message "pasted 2 items"
expect "$T/clip-many.out" tree_rows "recipes/|recipes/bread.md|*>recipes/groceries.txt|*recipes/journal.md|recipes/soup.md|recipes-copy/|recipes-copy-2/|recipes-copy-2/recipes/|recipes-copy-2/bread.md|recipes-copy-2/soup.md|trips/|trips/recipes/|trips/lisbon.md|garden-copy-2.txt|garden-copy.txt|garden.txt|groceries.txt|journal.md|letters.txt"
expect "$T/clip-many.out" file_ops "copy garden.txt > garden-copy.txt|copy garden.txt > garden-copy-2.txt|copy recipes > recipes-copy|copy recipes > recipes-copy-2|copy recipes > recipes-copy-2/recipes|copy recipes > trips/recipes|copy groceries.txt > recipes/groceries.txt|copy journal.md > recipes/journal.md"
for clip_dump in "$T"/clip-*.out; do
    clip_no_staging "$clip_dump"
done
for clip_item in garden.txt garden-copy.txt garden-copy-2.txt; do
    [ "$(cat "$T/clipproj/$clip_item" 2>/dev/null)" = "tomatoes in may" ] \
        || failed "clip: $clip_item is [$(cat "$T/clipproj/$clip_item" 2>/dev/null)], expected the original's text"
done
[ "$(cat "$T/clipproj/trips/recipes/bread.md" 2>/dev/null)" = "flour, water, salt" ] \
    || failed "clip: the folder was not pasted with what is in it"
[ -f "$T/clipproj/recipes/soup.md" ] || failed "clip: a copied folder lost a file"
[ "$(cat "$T/clipproj/recipes-copy/bread.md" 2>/dev/null)" = "flour, water, salt" ] \
    && [ "$(cat "$T/clipproj/recipes-copy-2/soup.md" 2>/dev/null)" = "leeks and potatoes" ] \
    || failed "clip: a folder pasted on its own row did not land beside itself, whole, as -copy and -copy-2"
[ ! -e "$T/clipproj/recipes-copy/recipes" ] \
    || failed "clip: the second paste went into the copy the first one made"
[ -f "$T/clipproj/recipes-copy-2/recipes/bread.md" ] \
    || failed "clip: a paste on a copy chosen on purpose did not go into it"
[ ! -e "$T/clipproj/recipes/recipes" ] && [ ! -e "$T/clipproj/trips/recipes/trips" ] \
    || failed "clip: a folder was pasted into itself"
[ "$(cat "$T/clipproj/recipes/journal.md" 2>/dev/null)" = "a quiet day" ] \
    && [ -f "$T/clipproj/recipes/groceries.txt" ] && [ -f "$T/clipproj/journal.md" ] \
    || failed "clip: two items copied together did not both arrive with the originals kept"
[ -z "$(clip_staging "$T/clipproj")" ] \
    || failed "clip: a folder a copy was built in is left: $(clip_staging "$T/clipproj")"

# Cut and paste. The context menu offers the four commands. A cut only dims
# the row: Escape calls it off and leaves the item on the clipboard as a
# copy; pasted where it already is, it stays. Unsaved work in the item's tab
# refuses the move, as it does a drag. Pasted elsewhere it moves, its tab
# and its local history with it, and the clipboard is empty after: there is
# nothing left to move twice. Duplicate puts a copy beside each item. A
# folder cut and pasted on its own row stays, as one pasted where it is.
clip_project "$T/cutproj"
mkdir -p "$T/cuthist"
cat > "$T/cut.script" <<SCRIPT
wait 500
click @sidebar.row.4
wait 200
text x
key 1 cmd s
wait 200
menuat @sidebar.row.4 0.5 0.5
dump $T/cut-menu.out
key 7 cmd x
dump $T/cut-cut.out
key 53
dump $T/cut-escape.out
key 7 cmd x
key 9 cmd v
dump $T/cut-stays.out
click 600 300
text y
menuat @sidebar.row.1 0.5 0.5
key 9 cmd v
dump $T/cut-dirty.out
key 1 cmd s
wait 200
key 9 cmd v
wait 300
wait 300
dump $T/cut-moved.out
click 600 300
text z
key 1 cmd s
wait 200
dump $T/cut-typed.out
menuat @sidebar.row.0 0.5 0.5
key 9 cmd v
dump $T/cut-again.out
click 150 650
clickat @sidebar.row.0 0.5 0.5 cmd
clickat @sidebar.row.4 0.5 0.5 cmd
action duplicateProjectItem:
wait 300
wait 300
dump $T/cut-duplicate.out
click 150 650
clickat @sidebar.row.0 0.5 0.5 cmd
key 7 cmd x
key 9 cmd v
dump $T/cut-folder-stays.out
quit
SCRIPT
CRC_HISTORY_DB="$T/cuthist/history.db" CRC_SELFTEST="$T/cut.script" "$BIN" "$T/cutproj" 2> "$T/cut.err"
clip_quiet "$T/cut.err"
expect "$T/cut-menu.out" message "menu: New File…|New Folder…|Cut|Copy|Paste|Duplicate|Copy Path|Copy Relative Path|Reveal in Finder|Rename…|Move to Trash"
expect "$T/cut-menu.out" tree_keys true
expect "$T/cut-cut.out" message "cut journal.md: paste to move it"
expect "$T/cut-cut.out" tree_rows "recipes/|trips/|garden.txt|groceries.txt|~*>journal.md|letters.txt"
expect "$T/cut-cut.out" clipboard_files "cut journal.md"
expect "$T/cut-cut.out" clipboard_text "journal.md"
expect "$T/cut-escape.out" message "cut called off"
expect "$T/cut-escape.out" tree_rows "recipes/|trips/|garden.txt|groceries.txt|*>journal.md|letters.txt"
expect "$T/cut-escape.out" tree_keys true
expect "$T/cut-escape.out" clipboard_files "copy journal.md"
expect "$T/cut-escape.out" file_ops ""
expect "$T/cut-stays.out" message "journal.md is already in this folder"
expect "$T/cut-stays.out" tree_rows "recipes/|trips/|garden.txt|groceries.txt|~*>journal.md|letters.txt"
expect "$T/cut-stays.out" file_ops ""
expect "$T/cut-dirty.out" message "save the affected tabs before moving this item"
expect "$T/cut-dirty.out" dirty true
expect "$T/cut-dirty.out" tree_rows "recipes/|*>trips/|garden.txt|groceries.txt|~journal.md|letters.txt"
expect "$T/cut-dirty.out" file_ops ""
expect "$T/cut-moved.out" message "moved journal.md"
expect "$T/cut-moved.out" tree_rows "recipes/|trips/|*>trips/journal.md|trips/lisbon.md|garden.txt|groceries.txt|letters.txt"
expect "$T/cut-moved.out" tabs "journal.md"
expect "$T/cut-moved.out" clipboard_files "none"
expect "$T/cut-moved.out" file_ops "move journal.md > trips/journal.md"
expect "$T/cut-typed.out" dirty false
expect "$T/cut-typed.out" window_title "journal.md"
expect "$T/cut-again.out" message "nothing to paste: the clipboard holds no files"
expect "$T/cut-duplicate.out" message "duplicated 2 items"
expect "$T/cut-duplicate.out" tree_rows "recipes/|*>recipes-copy/|trips/|trips/journal.md|trips/lisbon.md|*garden-copy.txt|garden.txt|groceries.txt|letters.txt"
expect "$T/cut-duplicate.out" file_ops "move journal.md > trips/journal.md|duplicate recipes > recipes-copy|duplicate garden.txt > garden-copy.txt"
# A folder cut and pasted on its own row is where it would go: it stays.
expect "$T/cut-folder-stays.out" message "recipes is already in this folder"
expect "$T/cut-folder-stays.out" tree_rows "~*>recipes/|recipes-copy/|trips/|trips/journal.md|trips/lisbon.md|garden-copy.txt|garden.txt|groceries.txt|letters.txt"
expect "$T/cut-folder-stays.out" file_ops "move journal.md > trips/journal.md|duplicate recipes > recipes-copy|duplicate garden.txt > garden-copy.txt"
for clip_dump in "$T"/cut-*.out; do
    clip_no_staging "$clip_dump"
done
[ ! -e "$T/cutproj/journal.md" ] || failed "cut: the cut file is still where it was after the paste"
case "$(cat "$T/cutproj/trips/journal.md" 2>/dev/null)" in
    *x*y*|*y*x*) ;;
    *) failed "cut: the moved file is [$(cat "$T/cutproj/trips/journal.md" 2>/dev/null)], expected what was typed before the move" ;;
esac
grep -q z "$T/cutproj/trips/journal.md" 2>/dev/null \
    || failed "cut: what was typed after the move was not saved to the moved file: the tab did not follow it"
# clip_history <path ending>: how many versions local history keeps for it.
clip_history() {
    /usr/bin/sqlite3 "$T/cuthist/history.db" "SELECT count(*) FROM snapshots s
        JOIN files f ON f.id = s.file WHERE f.path LIKE '%/cutproj/$1'" 2>&1
}
[ "$(clip_history journal.md)" = "0" ] \
    || failed "cut: local history still has [$(clip_history journal.md)] versions under the old path"
[ "$(clip_history trips/journal.md)" -ge 2 ] 2>/dev/null \
    || failed "cut: local history has [$(clip_history trips/journal.md)] versions under the new path, expected the ones kept before the move"
[ "$(cat "$T/cutproj/recipes-copy/bread.md" 2>/dev/null)" = "flour, water, salt" ] \
    && [ -f "$T/cutproj/recipes/bread.md" ] && [ -f "$T/cutproj/garden-copy.txt" ] && [ -f "$T/cutproj/garden.txt" ] \
    || failed "cut: Duplicate did not put a whole copy beside each item"
[ -z "$(clip_staging "$T/cutproj")" ] \
    || failed "cut: a folder a copy was built in is left: $(clip_staging "$T/cutproj")"

# Files copied in another app, the Finder for one, paste the same way: they
# are copied in. This copy is held (CRC_COPY_GATE: it waits for that file)
# so it can be looked at while it runs: the status line says it is copying,
# the folder it is built in is on disk and is not a row even after Refresh,
# and Escape in the tree stops it with nothing left. Let go the second time,
# it lands and both items are selected.
clip_project "$T/gateproj"
mkdir -p "$T/clipoutside/album" "$T/clipgate"
printf 'wish you were here\n' > "$T/clipoutside/postcard.txt"
printf 'summer\n' > "$T/clipoutside/album/cover.txt"
cat > "$T/gate.script" <<SCRIPT
wait 500
click 150 650
pasteboard $T/clipoutside/postcard.txt|$T/clipoutside/album
dump $T/gate-on.out
key 9 cmd v
wait 300
click @sidebar.action.3
wait 300
wait 300
click 150 650
dump $T/gate-held.out
wait 2000
key 53
wait 300
wait 300
dump $T/gate-stopped.out
wait 1000
key 9 cmd v
wait 200
dump $T/gate-held-again.out
touch $T/clipgate/open
wait 400
wait 400
dump $T/gate-landed.out
quit
SCRIPT
CRC_COPY_GATE="$T/clipgate/open" CRC_SELFTEST="$T/gate.script" "$BIN" "$T/gateproj" 2> "$T/gate.err" &
clip_pid=$!
for _ in $(seq 1 100); do
    [ -f "$T/gate-held.out" ] && break
    sleep 0.1
done
clip_seen=$(clip_staging "$T/gateproj")
for _ in $(seq 1 100); do
    [ -f "$T/gate-stopped.out" ] && break
    sleep 0.1
done
clip_after=$(clip_staging "$T/gateproj")
wait "$clip_pid" 2>/dev/null || true
clip_quiet "$T/gate.err"
[ -n "$clip_seen" ] || failed "gate: the held copy had no folder on disk to be built in: the check that it is hidden checked nothing"
[ -z "$clip_after" ] || failed "gate: Escape left the folder the copy was built in: $clip_after"
expect "$T/gate-on.out" clipboard_files "copy $T/clipoutside/postcard.txt|$T/clipoutside/album"
expect "$T/gate-on.out" clipboard_text ""
expect "$T/gate-held.out" message "Copying 2 items…"
expect "$T/gate-held.out" file_job "copying 2"
expect "$T/gate-held.out" tree_rows "recipes/|trips/|garden.txt|groceries.txt|journal.md|letters.txt"
expect "$T/gate-held.out" finder_entries 7
expect "$T/gate-stopped.out" message "copy stopped: nothing was copied"
expect "$T/gate-stopped.out" file_job "none"
expect "$T/gate-stopped.out" tree_rows "recipes/|trips/|garden.txt|groceries.txt|journal.md|letters.txt"
expect "$T/gate-stopped.out" file_ops ""
expect "$T/gate-held-again.out" message "Copying 2 items…"
expect "$T/gate-landed.out" message "pasted 2 items"
expect "$T/gate-landed.out" file_job "none"
expect "$T/gate-landed.out" tree_rows "*>album/|recipes/|trips/|garden.txt|groceries.txt|journal.md|letters.txt|*postcard.txt"
expect "$T/gate-landed.out" file_ops "copy $T/clipoutside/postcard.txt > postcard.txt|copy $T/clipoutside/album > album"
for clip_dump in "$T"/gate-*.out; do
    clip_no_staging "$clip_dump"
done
[ "$(cat "$T/gateproj/postcard.txt" 2>/dev/null)" = "wish you were here" ] \
    && [ "$(cat "$T/gateproj/album/cover.txt" 2>/dev/null)" = "summer" ] \
    || failed "gate: the files from the other app were not copied in"
[ -f "$T/clipoutside/postcard.txt" ] && [ -f "$T/clipoutside/album/cover.txt" ] \
    || failed "gate: a paste from another app took the originals"
[ -z "$(clip_staging "$T/gateproj")" ] \
    || failed "gate: a folder a copy was built in is left: $(clip_staging "$T/gateproj")"

# Quit while a copy runs: it is stopped first, so no item is left half
# copied under its name and no folder it was built in stays behind.
clip_project "$T/clipquit"
mkdir -p "$T/clipshut"
cat > "$T/clipquit.script" <<SCRIPT
wait 500
click 150 650
pasteboard $T/clipoutside/album
key 9 cmd v
wait 300
dump $T/clipquit-held.out
quit
SCRIPT
CRC_COPY_GATE="$T/clipshut/open" CRC_SELFTEST="$T/clipquit.script" "$BIN" "$T/clipquit" 2> "$T/clipquit.err"
clip_quiet "$T/clipquit.err"
expect "$T/clipquit-held.out" message "Copying 1 item…"
expect "$T/clipquit-held.out" file_job "copying 1"
[ ! -e "$T/clipquit/album" ] || failed "quit: an item whose copy was not finished is there under its name"
[ -z "$(clip_staging "$T/clipquit")" ] \
    || failed "quit: the folder the copy was built in is left: $(clip_staging "$T/clipquit")"
