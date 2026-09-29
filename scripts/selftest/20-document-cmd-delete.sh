# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh document-cmd-delete.

# ---- Cmd-Delete in the document edits the document, not the tree -----------
# The tree kept its selection after a file row was clicked, and that was
# enough to enable Move to Trash: Cmd-Delete while typing trashed the file.
# The file's row is clicked, which opens it; Cmd-Delete then goes to the
# document, straight away and again after typing.
mkdir -p "$T/docdelproj"
printf 'keep me\n' > "$T/docdelproj/a.txt"
cat > "$T/docdel.script" <<SCRIPT
click @sidebar.row.0
wait 300
key 51 cmd
wait 200
key 124
text xyz
key 51 cmd
wait 200
dump $T/docdel.out
quit
SCRIPT
CRC_SELFTEST="$T/docdel.script" "$BIN" "$T/docdelproj" 2> "$T/docdel.err"
expect "$T/docdel.out" tabs "a.txt"
expect_line "$T/docdel.out" 1 "eep me"
if [ ! -f "$T/docdelproj/a.txt" ]; then
    echo "FAIL: Cmd-Delete in the document moved the open file to Trash"
    fail=1
fi
