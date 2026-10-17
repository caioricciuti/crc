# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh table-view.

# ---- table view ----------------------------------------------------------------
# A CSV of 300 records (a header and 299 rows) with quoted cells holding
# commas, doubled quotes and a line break, and two short rows, opens as a
# grid. With nothing selected the arrow and page keys scroll the body and
# Home and End go to the ends. A click in the gutter selects the record's
# first cell; the arrows, Tab (wrapping to the next record), Shift-Tab,
# Home, End, Cmd-Home and Cmd-End then move the selection, which the
# status line shows as Row and Col and which scrolls into view. Text in
# the strip shows the file as text and Grid brings the grid back with the
# selection kept; Escape drops the selection, then leaves for the text.
# Editing a cell is 85-table-edit.sh. SELFTEST_FRAMES, when set, is a
# folder the grid's frame is copied to.
mkdir -p "$T/tableproj"
{
    printf 'id,name,note,amount\n'
    printf '1,Ada,plain,1.00\n'
    printf '2,"Smith, Jane","says ""hi""",10.50\n'
    printf '3,Ann,"line one\nline two",3.25\n'
    printf '4,Bob\n'
    printf '5,Cy,short row\n'
    i=6
    while [ "$i" -le 299 ]; do
        printf '%s,Person %s,note %s,%s.00\n' "$i" "$i" "$i" "$i"
        i=$((i + 1))
    done
} > "$T/tableproj/people.csv"
[ "$(wc -l < "$T/tableproj/people.csv" | tr -d ' ')" = 301 ] \
    || failed "table: the fixture has $(wc -l < "$T/tableproj/people.csv") lines, expected 301"
cat > "$T/table.script" <<SCRIPT
wait 800
wait 400
dump $T/table-open.out
key 125
key 125
key 121
wait 200
dump $T/table-scrolled.out
key 119
wait 200
dump $T/table-end.out
key 115
wait 200
dump $T/table-home.out
clickin @table.body 20 5
wait 200
dump $T/table-sel.out
key 124
key 125
key 48
key 48
key 48
wait 200
dump $T/table-tab.out
key 48 shift
key 126
key 126
key 126
wait 200
dump $T/table-up.out
key 115
key 119 cmd
wait 200
dump $T/table-last.out
key 115 cmd
key 125
key 124
wait 200
dump $T/table-cell.out
frame $T/table.bmp
click @table.mode.text
wait 300
dump $T/table-text.out
click @table.mode.grid
wait 300
dump $T/table-grid.out
key 53
wait 200
dump $T/table-esc1.out
key 53
wait 200
dump $T/table-esc.out
quit
SCRIPT
CRC_SELFTEST="$T/table.script" "$BIN" "$T/tableproj/people.csv" 2> "$T/table.err"
expect "$T/table-open.out" table "grid rows=300 cols=4 scroll=0 sel=- editing=false"
case "$(grep -m1 '^table: ' "$T/table-scrolled.out")" in
    "table: grid rows=300 cols=4 scroll=0 sel="*) failed "table: Down and Page Down did not scroll" ;;
    "table: grid rows=300 cols=4 scroll="*) ;;
    *) failed "table: after the keys: $(grep -m1 '^table: ' "$T/table-scrolled.out")" ;;
esac
# End: the last body row is the last on screen, so the scroll is 299 less
# the rows that fit, which the window's height decides.
case "$(grep -m1 '^table: ' "$T/table-end.out")" in
    "table: grid rows=300 cols=4 scroll=0 sel="*) failed "table: End did not scroll" ;;
    "table: grid rows=300 cols=4 scroll="*) ;;
    *) failed "table: after End: $(grep -m1 '^table: ' "$T/table-end.out")" ;;
esac
expect "$T/table-home.out" table "grid rows=300 cols=4 scroll=0 sel=- editing=false"
# The click lands in the gutter beside the header (past the sidebar
# divider's grab zone): its first cell. Right, Down and three Tabs from
# there: the last Tab wraps to the next record.
expect "$T/table-sel.out" table "grid rows=300 cols=4 scroll=0 sel=0,0 editing=false"
expect "$T/table-tab.out" table "grid rows=300 cols=4 scroll=0 sel=2,0 editing=false"
# Shift-Tab wraps back, and Up stops at the header.
expect "$T/table-up.out" table "grid rows=300 cols=4 scroll=0 sel=0,3 editing=false"
# Home, then Cmd-End: the last record, scrolled into view.
case "$(grep -m1 '^table: ' "$T/table-last.out")" in
    "table: grid rows=300 cols=4 scroll=0 sel="*) failed "table: Cmd-End did not scroll" ;;
    "table: grid rows=300 cols=4 scroll="*" sel=299,0 editing=false") ;;
    *) failed "table: after Cmd-End: $(grep -m1 '^table: ' "$T/table-last.out")" ;;
esac
# Cmd-Home brings the body back to the top; Down and Right pick a cell,
# which the frame shows outlined and the status line as Row 2, Col 2.
expect "$T/table-cell.out" table "grid rows=300 cols=4 scroll=0 sel=1,1 editing=false"
expect "$T/table-text.out" table "text"
expect_line "$T/table-text.out" 1 "id,name,note,amount"
expect_line "$T/table-text.out" 3 '2,"Smith, Jane","says ""hi""",10.50'
expect "$T/table-grid.out" table "grid rows=300 cols=4 scroll=0 sel=1,1 editing=false"
expect "$T/table-esc1.out" table "grid rows=300 cols=4 scroll=0 sel=- editing=false"
expect "$T/table-esc.out" table "text"
[ -s "$T/table.bmp" ] || failed "table: no frame was written"
[ -z "${SELFTEST_FRAMES:-}" ] || cp -f "$T/table.bmp" "$SELFTEST_FRAMES/"
