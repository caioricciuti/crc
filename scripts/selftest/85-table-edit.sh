# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh table-edit.

# ---- table cell editing --------------------------------------------------------
# A CRLF CSV of a header and three records, one of them short, opens as a
# grid. Return on the selected cell opens a field holding its value;
# typing a comma into it and Return commits the value quoted, as one edit
# the document is dirty by, and Cmd-Z takes back to the exact old text.
# Typing on a selected cell replaces its value; Tab commits and moves
# right, Shift-Tab left; Escape drops the field with the cell as it was. A
# cell a short record does not have is padded into being. Committing the
# value a cell already holds edits nothing. A double click opens the cell
# under it and a click outside the grid commits. Cmd-S writes the file
# with its own line endings. SELFTEST_FRAMES, when set, is a folder the
# frame with the field open is copied to.
mkdir -p "$T/tableedit"
printf 'id,name,note\r\n1,Ada,plain\r\n2,Bob\r\n3,Cy,"x, y"\r\n' > "$T/tableedit/people.csv"
cat > "$T/table-edit.script" <<SCRIPT
wait 800
wait 400
clickin @table.body 20 5
key 125
key 124
key 36
wait 200
dump $T/edit-open.out
text , Lovelace
wait 200
frame $T/table-edit.bmp
key 36
wait 300
dump $T/edit-commit.out
key 6 cmd z
wait 300
dump $T/edit-undo.out
text Grace
wait 200
dump $T/edit-typing.out
key 48
wait 300
dump $T/edit-tab.out
key 36
text zzz
key 53
wait 200
dump $T/edit-escape.out
key 36
text x
key 48 shift
wait 300
dump $T/edit-shift-tab.out
key 125
key 124
text hi
key 36
wait 300
dump $T/edit-pad.out
key 36
key 36
wait 300
dump $T/edit-same.out
clickin @table.body 20 5 2
wait 200
dump $T/edit-dblclick.out
text x
click @table.mode.grid
wait 300
dump $T/edit-click-away.out
key 1 cmd s
wait 600
dump $T/edit-saved.out
quit
SCRIPT
CRC_SELFTEST="$T/table-edit.script" "$BIN" "$T/tableedit/people.csv" 2> "$T/table-edit.err"
# The gutter click selects the header's first cell; Down and Right go to
# Ada, and Return opens the field on it.
expect "$T/edit-open.out" table "grid rows=4 cols=3 scroll=0 sel=1,1 editing=true"
expect "$T/edit-open.out" dirty "false"
# ", Lovelace" typed after the value and Return: the cell is quoted for
# its comma, the document dirty, the field closed, the selection kept.
expect "$T/edit-commit.out" table "grid rows=4 cols=3 scroll=0 sel=1,1 editing=false"
expect_line "$T/edit-commit.out" 2 '1,"Ada, Lovelace",plain'
expect "$T/edit-commit.out" dirty "true"
# Cmd-Z: one step back to the old text.
expect_line "$T/edit-undo.out" 2 '1,Ada,plain'
expect "$T/edit-undo.out" table "grid rows=4 cols=3 scroll=0 sel=1,1 editing=false"
# Typing on the cell opens the field empty, with the typed text.
expect "$T/edit-typing.out" table "grid rows=4 cols=3 scroll=0 sel=1,1 editing=true"
expect_line "$T/edit-typing.out" 2 '1,Ada,plain'
# Tab commits and moves right.
expect "$T/edit-tab.out" table "grid rows=4 cols=3 scroll=0 sel=1,2 editing=false"
expect_line "$T/edit-tab.out" 2 '1,Grace,plain'
# Escape after typing: nothing changes.
expect "$T/edit-escape.out" table "grid rows=4 cols=3 scroll=0 sel=1,2 editing=false"
expect_line "$T/edit-escape.out" 2 '1,Grace,plain'
# Return opens with the value, "x" goes after it, Shift-Tab commits and
# moves left.
expect "$T/edit-shift-tab.out" table "grid rows=4 cols=3 scroll=0 sel=1,1 editing=false"
expect_line "$T/edit-shift-tab.out" 2 '1,Grace,plainx'
# The short record gets its missing delimiter and the cell.
expect "$T/edit-pad.out" table "grid rows=4 cols=3 scroll=0 sel=2,2 editing=false"
expect_line "$T/edit-pad.out" 3 '2,Bob,hi'
# Return, Return on the cell: the same value, no edit, no undo step.
expect_line "$T/edit-same.out" 3 '2,Bob,hi'
pad_undo=$(grep -m1 '^undo: ' "$T/edit-pad.out" || true)
same_undo=$(grep -m1 '^undo: ' "$T/edit-same.out" || true)
[ -n "$pad_undo" ] && [ "$pad_undo" = "$same_undo" ] \
    || failed "table-edit: committing an unchanged value took an undo step: [$pad_undo] then [$same_undo]"
# A double click on the header's gutter opens its first cell.
expect "$T/edit-dblclick.out" table "grid rows=4 cols=3 scroll=0 sel=0,0 editing=true"
# "x" typed after "id", then a click on the strip: committed.
expect "$T/edit-click-away.out" table "grid rows=4 cols=3 scroll=0 sel=0,0 editing=false"
expect_line "$T/edit-click-away.out" 1 'idx,name,note'
# Saved with the file's CRLF endings.
expect "$T/edit-saved.out" dirty "false"
printf 'idx,name,note\r\n1,Grace,plainx\r\n2,Bob,hi\r\n3,Cy,"x, y"\r\n' > "$T/tableedit/expected.csv"
cmp -s "$T/tableedit/people.csv" "$T/tableedit/expected.csv" \
    || failed "table-edit: the saved file differs: $(od -c "$T/tableedit/people.csv" | head -5)"
[ -s "$T/table-edit.bmp" ] || failed "table-edit: no frame was written"
[ -z "${SELFTEST_FRAMES:-}" ] || cp -f "$T/table-edit.bmp" "$SELFTEST_FRAMES/"
