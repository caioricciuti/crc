# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh table-rows-columns.

# ---- table rows and columns ----------------------------------------------------
# A CSV of a header and three records opens as a grid. With a cell
# selected, the Table commands in the palette insert a record above or
# below it, delete its record, insert a column left or right of it, delete
# its column and clear it, each as one edit of the text and one undo step
# that Cmd-Z takes back whole. The dump's rows and cols follow each edit,
# the selection lands on the new record's first cell, the new column's
# cell, or what is now where a deleted one was. Delete and Backspace on a
# selected cell clear it. SELFTEST_FRAMES, when set, is a folder the frame
# with the inserted column is copied to.
mkdir -p "$T/tablerows"
printf 'id,name,note\n1,Ada,plain\n2,Bob,b\n3,Cy,c\n' > "$T/tablerows/people.csv"
cat > "$T/table-rows.script" <<SCRIPT
wait 800
wait 400
clickin @table.body 20 5
key 125
key 124
wait 200
dump $T/rows-start.out
key 35 cmd p
wait 200
text >insert row above
wait 200
dump $T/rows-palette.out
key 36
wait 300
dump $T/rows-above.out
key 6 cmd z
wait 300
dump $T/rows-above-undo.out
key 124
key 35 cmd p
wait 200
text >insert row below
key 36
wait 300
dump $T/rows-below.out
key 6 cmd z
wait 300
dump $T/rows-below-undo.out
key 115 cmd
key 125
key 124
key 35 cmd p
wait 200
text >delete row
key 36
wait 300
dump $T/rows-delete.out
key 6 cmd z
wait 300
dump $T/rows-delete-undo.out
key 35 cmd p
wait 200
text >insert column left
key 36
wait 300
dump $T/cols-left.out
frame $T/table-columns.bmp
key 6 cmd z
wait 300
dump $T/cols-left-undo.out
key 35 cmd p
wait 200
text >insert column right
key 36
wait 300
dump $T/cols-right.out
key 6 cmd z
wait 300
dump $T/cols-right-undo.out
key 124
key 35 cmd p
wait 200
text >delete column
key 36
wait 300
dump $T/cols-delete.out
key 6 cmd z
wait 300
dump $T/cols-delete-undo.out
key 35 cmd p
wait 200
text >clear cell
key 36
wait 300
dump $T/cell-clear.out
key 6 cmd z
wait 300
dump $T/cell-clear-undo.out
key 51
wait 300
dump $T/cell-backspace.out
key 6 cmd z
wait 300
key 117
wait 300
dump $T/cell-delete.out
key 6 cmd z
wait 300
dump $T/cell-delete-undo.out
quit
SCRIPT
CRC_SELFTEST="$T/table-rows.script" "$BIN" "$T/tablerows/people.csv" 2> "$T/table-rows.err"
# The gutter click selects the header's first cell; Down and Right go to
# Ada.
expect "$T/rows-start.out" table "grid rows=4 cols=3 scroll=0 sel=1,1 editing=false"
# The palette lists the command by its name, in the Table menu.
expect "$T/rows-palette.out" palette_first "Insert Row Above"
# An empty record above Ada's, selected at its first cell.
expect "$T/rows-above.out" table "grid rows=5 cols=3 scroll=0 sel=1,0 editing=false"
expect_line "$T/rows-above.out" 2 ',,'
expect_line "$T/rows-above.out" 3 '1,Ada,plain'
expect "$T/rows-above.out" dirty "true"
# Cmd-Z: the old text in one step, the selection where it was.
expect "$T/rows-above-undo.out" table "grid rows=4 cols=3 scroll=0 sel=1,0 editing=false"
expect_line "$T/rows-above-undo.out" 2 '1,Ada,plain'
expect_line "$T/rows-above-undo.out" 3 '2,Bob,b'
expect "$T/rows-above-undo.out" dirty "false"
# Right, then an empty record below Ada's.
expect "$T/rows-below.out" table "grid rows=5 cols=3 scroll=0 sel=2,0 editing=false"
expect_line "$T/rows-below.out" 2 '1,Ada,plain'
expect_line "$T/rows-below.out" 3 ',,'
expect_line "$T/rows-below.out" 4 '2,Bob,b'
# Undo leaves the selection where it is: on Bob's record, now at the new
# one's place.
expect "$T/rows-below-undo.out" table "grid rows=4 cols=3 scroll=0 sel=2,0 editing=false"
expect_line "$T/rows-below-undo.out" 3 '2,Bob,b'
# Cmd-Home, Down and Right go back to Ada's name; Delete Row takes the
# record and its line ending, and Bob's record is selected in its place,
# in the same column.
expect "$T/rows-delete.out" table "grid rows=3 cols=3 scroll=0 sel=1,1 editing=false"
expect_line "$T/rows-delete.out" 1 'id,name,note'
expect_line "$T/rows-delete.out" 2 '2,Bob,b'
expect_line "$T/rows-delete.out" 3 '3,Cy,c'
expect "$T/rows-delete-undo.out" table "grid rows=4 cols=3 scroll=0 sel=1,1 editing=false"
expect_line "$T/rows-delete-undo.out" 2 '1,Ada,plain'
# A column left of name, in every record, the header included: the new
# empty cell is selected.
expect "$T/cols-left.out" table "grid rows=4 cols=4 scroll=0 sel=1,1 editing=false"
expect_line "$T/cols-left.out" 1 'id,,name,note'
expect_line "$T/cols-left.out" 2 '1,,Ada,plain'
expect_line "$T/cols-left.out" 4 '3,,Cy,c'
expect "$T/cols-left-undo.out" table "grid rows=4 cols=3 scroll=0 sel=1,1 editing=false"
expect_line "$T/cols-left-undo.out" 1 'id,name,note'
# A column right of name.
expect "$T/cols-right.out" table "grid rows=4 cols=4 scroll=0 sel=1,2 editing=false"
expect_line "$T/cols-right.out" 1 'id,name,,note'
expect_line "$T/cols-right.out" 3 '2,Bob,,b'
expect "$T/cols-right-undo.out" table "grid rows=4 cols=3 scroll=0 sel=1,2 editing=false"
expect_line "$T/cols-right-undo.out" 1 'id,name,note'
# Right stays on the last column; Delete Column takes note out of every
# record and the selection moves in to the last column left.
expect "$T/cols-delete.out" table "grid rows=4 cols=2 scroll=0 sel=1,1 editing=false"
expect_line "$T/cols-delete.out" 1 'id,name'
expect_line "$T/cols-delete.out" 2 '1,Ada'
expect_line "$T/cols-delete.out" 4 '3,Cy'
expect "$T/cols-delete-undo.out" table "grid rows=4 cols=3 scroll=0 sel=1,1 editing=false"
expect_line "$T/cols-delete-undo.out" 2 '1,Ada,plain'
# Clear Cell empties Ada's name and keeps the cell selected.
expect "$T/cell-clear.out" table "grid rows=4 cols=3 scroll=0 sel=1,1 editing=false"
expect_line "$T/cell-clear.out" 2 '1,,plain'
expect_line "$T/cell-clear-undo.out" 2 '1,Ada,plain'
# Backspace and Delete on the cell do the same, each undone in one step.
expect "$T/cell-backspace.out" table "grid rows=4 cols=3 scroll=0 sel=1,1 editing=false"
expect_line "$T/cell-backspace.out" 2 '1,,plain'
expect_line "$T/cell-delete.out" 2 '1,,plain'
expect_line "$T/cell-delete-undo.out" 2 '1,Ada,plain'
expect "$T/cell-delete-undo.out" dirty "false"
[ -s "$T/table-columns.bmp" ] || failed "table-rows: no frame was written"
[ -z "${SELFTEST_FRAMES:-}" ] || cp -f "$T/table-columns.bmp" "$SELFTEST_FRAMES/"
