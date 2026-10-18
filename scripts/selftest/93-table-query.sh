# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh table-query.

# ---- table query -----------------------------------------------------------------
# A CSV opens as a grid with the strip's Query segment. Clicking it opens
# the SQL field; Cmd-Return runs the statement through the fake duckdb
# (scripts/fake-duckdb.py, chosen by CRC_DUCKDB_FAKE) and the CSV it
# answers with lands in a read-only "Result: people.csv" tab drawn as a
# grid, which refuses a cell edit. Back to file returns to the CSV with the
# field open on the same statement; a failing statement puts DuckDB's
# error in the strip, opens no tab and keeps the field's text, which also
# survives Escape. A second launch with no DuckDB at all says so in the
# status line. SELFTEST_FRAMES, when set, is a folder the frames with the
# field open and with the result tab are copied to.
mkdir -p "$T/tablequery"
printf 'id,name\n1,Ada\n2,Bob\n3,Cy\n' > "$T/tablequery/people.csv"
cat > "$T/table-query.script" <<SCRIPT
wait 800
wait 400
dump $T/query-start.out
click @table.query
wait 200
dump $T/query-open.out
text SELECT * FROM here
wait 200
frame $T/table-query.bmp
dump $T/query-typed.out
key 36 cmd
wait 600
dump $T/query-result.out
frame $T/table-query-result.bmp
clickin @table.body 20 5
key 125
key 124
text zzz
key 36
wait 300
dump $T/query-readonly.out
click @table.export
wait 200
dump $T/query-export.out
click @table.back
wait 300
dump $T/query-back.out
key 0 cmd a
text SELECT nope
key 36 cmd
wait 600
dump $T/query-error.out
key 53
wait 200
dump $T/query-escape.out
click @table.query
wait 200
dump $T/query-reopen.out
quit
SCRIPT
CRC_QUERY_EXPORT="$T/query-export.csv" CRC_DUCKDB_FAKE="$PWD/scripts/fake-duckdb.py" CRC_SELFTEST="$T/table-query.script" \
    "$BIN" "$T/tablequery/people.csv" 2> "$T/table-query.err"
# The grid, no field, nothing run.
expect "$T/query-start.out" table "grid rows=4 cols=2 scroll=0 sel=- editing=false"
expect "$T/query-start.out" query "idle"
expect "$T/query-start.out" query_sql "-"
# The Query segment opens the field, empty.
expect "$T/query-open.out" query_sql ""
expect "$T/query-typed.out" query_sql "SELECT * FROM here"
expect "$T/query-typed.out" query "idle"
# Cmd-Return: the result tab, in front, a grid of the fake's three rows
# under the header, and the query line on it.
expect "$T/query-result.out" tabs "people.csv | Result: people.csv"
expect "$T/query-result.out" active "1"
expect "$T/query-result.out" table "grid rows=4 cols=2 scroll=0 sel=- editing=false"
expect "$T/query-result.out" query "result rows=3 truncated=false"
expect_line "$T/query-result.out" 1 'a,b'
expect_line "$T/query-result.out" 2 '1,x'
# The result is read and never edited: typing on a cell and Return
# changes nothing and says why.
expect "$T/query-readonly.out" table "grid rows=4 cols=2 scroll=0 sel=1,1 editing=false"
expect_line "$T/query-readonly.out" 2 '1,x'
expect "$T/query-readonly.out" message "this document is read-only"
expect "$T/query-readonly.out" dirty "false"
# Back to file: the CSV in front, its field open on the statement.
expect "$T/query-back.out" active "0"
expect "$T/query-back.out" tabs "people.csv | Result: people.csv"
expect "$T/query-back.out" query_sql "SELECT * FROM here"
expect "$T/query-back.out" query "idle"
# A statement DuckDB refuses: the error in the strip, no new tab, the
# field still holding the text.
expect "$T/query-error.out" query "error Catalog Error: Table with name nope does not exist! | LINE 1: SELECT * FROM nope"
expect "$T/query-error.out" tabs "people.csv | Result: people.csv"
expect "$T/query-error.out" active "0"
expect "$T/query-error.out" query_sql "SELECT nope"
# Escape closes the field; the segment reopens it with the text kept.
expect "$T/query-escape.out" query_sql "-"
expect "$T/query-reopen.out" query_sql "SELECT nope"
[ -s "$T/table-query.bmp" ] || failed "table-query: no frame was written"
[ -s "$T/table-query-result.bmp" ] || failed "table-query: no result frame was written"
[ -z "${SELFTEST_FRAMES:-}" ] || cp -f "$T/table-query.bmp" "$T/table-query-result.bmp" "$SELFTEST_FRAMES/"

# Without DuckDB (CRC_DUCKDB_FAKE set but empty finds no tool, whatever
# the machine has): Run says what to install, and opens no tab.
cat > "$T/table-query-none.script" <<SCRIPT
wait 800
wait 400
click @table.query
wait 200
text SELECT 42
key 36 cmd
wait 300
dump $T/query-none.out
quit
SCRIPT
CRC_DUCKDB_FAKE= CRC_SELFTEST="$T/table-query-none.script" \
    "$BIN" "$T/tablequery/people.csv" 2> "$T/table-query-none.err"
expect "$T/query-none.out" message "DuckDB is not installed (brew install duckdb)"
expect "$T/query-none.out" tabs "people.csv"
expect "$T/query-none.out" query "idle"
expect "$T/query-none.out" query_sql "SELECT 42"

expect "$T/query-export.out" message "exported CSV"
expect "$T/query-export.out" dirty "false"
printf 'a,b\n1,x\n2,y\n3,z\n' > "$T/query-export-expected.csv"
cmp "$T/query-export.csv" "$T/query-export-expected.csv" || failed "query export differs from result"
