# Read-only grids go through the installed-tool worker, including launch.
# Fixture bytes are deliberately fake: the fake tool supplies the CSV.
mkdir -p "$T/datapreview"
for extension in parquet jsonl ndjson sqlite db; do
    printf 'fixture\n' > "$T/datapreview/sample.$extension"
    cat > "$T/data-preview.script" <<SCRIPT
wait 1200
wait 600
dump $T/data-$extension.out
clickin @table.body 20 5
text zzz
key 36
wait 200
dump $T/data-readonly-$extension.out
click @table.query
wait 200
text SELECT * FROM here
key 36 cmd
wait 600
dump $T/data-query-$extension.out
click @table.back
wait 200
dump $T/data-back-$extension.out
quit
SCRIPT
    CRC_DUCKDB_FAKE="$PWD/scripts/fake-duckdb.py" CRC_SELFTEST="$T/data-preview.script" \
        "$BIN" "$T/datapreview/sample.$extension" 2> "$T/data-$extension.err"
    expect "$T/data-$extension.out" tabs "sample.$extension"
    expect "$T/data-$extension.out" table "grid rows=4 cols=2 scroll=0 sel=- editing=false"
    expect "$T/data-$extension.out" dirty "false"
    expect "$T/data-readonly-$extension.out" dirty "false"
    expect "$T/data-readonly-$extension.out" message "this document is read-only"
    expect_line "$T/data-readonly-$extension.out" 2 '1,x'
    expect "$T/data-query-$extension.out" tabs "sample.$extension | Result: sample.$extension"
    expect "$T/data-query-$extension.out" query "result rows=3 truncated=false"
    expect "$T/data-back-$extension.out" active "0"
    expect "$T/data-back-$extension.out" query_sql "SELECT * FROM here"
    printf 'fixture\n' > "$T/data-original"
    cmp "$T/datapreview/sample.$extension" "$T/data-original" || failed "data preview changed its source"
done
