# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh palette-cmd-delete.

# ---- Cmd-Delete in the palette edits the query, not the tree ---------------
# The File menu's Move to Trash is Cmd-Delete. With the palette open and a
# sidebar row selected it used to fire on that row. The row is selected by
# clicking it first; the palette then gets Cmd-Delete and Option-Delete.
mkdir -p "$T/trashproj"
printf 'keep me\n' > "$T/trashproj/caio.txt"
printf 'GET http://127.0.0.1:1/\n' > "$T/trashproj/test.http"
cat > "$T/trash.script" <<SCRIPT
click @sidebar.row.0
wait 200
key 35 cmd p
wait 200
text te st
key 51 opt
dump $T/trash-word.out
key 51 cmd
wait 200
dump $T/trash.out
quit
SCRIPT
CRC_SELFTEST="$T/trash.script" "$BIN" "$T/trashproj" 2> "$T/trash.err"
expect "$T/trash-word.out" palette_query "te "
expect "$T/trash.out" palette_query ""
if [ ! -f "$T/trashproj/caio.txt" ]; then
    echo "FAIL: Cmd-Delete in the palette moved the selected file to Trash"
    fail=1
fi
