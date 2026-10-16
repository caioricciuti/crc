# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh persistent-undo.

# ---- persistent undo: a clean file's undo outlives the window -----------------
# notes.txt is edited and saved twice, two undo steps, and crc quits. Opened
# again, its undo is back without a keystroke: Cmd-Z gives the first save's
# text, unsaved, and Cmd-Shift-Z the second, clean. A tab still unsaved at
# quit leaves the journal of its last clean text alone. Changed on disk
# between runs, the file opens with nothing to undo. A .env file is a
# secret: saved with changes to undo, no journal is kept for it.
mkdir -p "$T/puproj" "$T/pu"
pudb="$T/pu/history.db"
printf 'first\n' > "$T/puproj/notes.txt"
printf 'A=1\n' > "$T/puproj/.env"

# pu_run <name> <file>: crc on <file> with $T/pu-<name>.script.
pu_run() {
    CRC_HISTORY_DB="$pudb" CRC_DISCARD_ANSWER=dont-save \
        CRC_SELFTEST="$T/pu-$1.script" "$BIN" "$2" 2> "$T/pu-$1.err"
}

cat > "$T/pu-edit.script" <<SCRIPT
wait 800
text one;
key 1 cmd s
wait 300
text two;
key 1 cmd s
wait 300
dump $T/pu-edit.out
quit
SCRIPT
pu_run edit "$T/puproj/notes.txt"
[ "$(cat "$T/puproj/notes.txt")" = "one;two;first" ] \
    || failed "pu: notes.txt was not saved: $(cat "$T/puproj/notes.txt")"
expect "$T/pu-edit.out" undo "2/0"

cat > "$T/pu-back.script" <<SCRIPT
wait 800
wait 300
dump $T/pu-restored.out
key 6 cmd z
wait 200
dump $T/pu-undone.out
key 6 cmd,shift Z
wait 200
dump $T/pu-redone.out
quit
SCRIPT
pu_run back "$T/puproj/notes.txt"
expect "$T/pu-restored.out" undo "2/0"
expect "$T/pu-restored.out" dirty "false"
expect_line "$T/pu-restored.out" 1 "one;two;first"
expect "$T/pu-undone.out" undo "1/1"
expect "$T/pu-undone.out" dirty "true"
expect_line "$T/pu-undone.out" 1 "one;first"
expect "$T/pu-redone.out" undo "2/0"
expect "$T/pu-redone.out" dirty "false"
expect_line "$T/pu-redone.out" 1 "one;two;first"

# Unsaved at quit: the quit snapshot, and the journal as it was.
cat > "$T/pu-dirty.script" <<SCRIPT
wait 800
wait 300
text x
wait 200
dump $T/pu-dirty.out
quit
SCRIPT
pu_run dirty "$T/puproj/notes.txt"
expect "$T/pu-dirty.out" dirty "true"
expect_line "$T/pu-dirty.out" 1 "xone;two;first"
[ "$(cat "$T/puproj/notes.txt")" = "one;two;first" ] \
    || failed "pu: notes.txt was written: $(cat "$T/puproj/notes.txt")"
cat > "$T/pu-again.script" <<SCRIPT
wait 800
wait 300
dump $T/pu-again.out
quit
SCRIPT
pu_run again "$T/puproj/notes.txt"
expect "$T/pu-again.out" undo "2/0"
expect_line "$T/pu-again.out" 1 "one;two;first"

# Changed on disk since: the journal is for another text.
printf 'changed\n' > "$T/puproj/notes.txt"
pu_run again "$T/puproj/notes.txt"
expect "$T/pu-again.out" undo "0/0"
expect_line "$T/pu-again.out" 1 "changed"

cat > "$T/pu-env.script" <<SCRIPT
wait 800
text B
key 1 cmd s
wait 300
text C
key 1 cmd s
wait 300
quit
SCRIPT
pu_run env "$T/puproj/.env"
[ "$(cat "$T/puproj/.env")" = "BCA=1" ] \
    || failed "pu: .env was not saved: $(cat "$T/puproj/.env")"
pu_run again "$T/puproj/.env"
expect "$T/pu-again.out" undo "0/0"
expect_line "$T/pu-again.out" 1 "BCA=1"
# pu_journals <file name>: undo rows kept for it.
pu_journals() {
    /usr/bin/sqlite3 "$pudb" "SELECT count(*) FROM undo u JOIN files f ON f.id = u.file
        WHERE f.path LIKE '%/puproj/$1'" 2>&1
}
[ "$(pu_journals notes.txt)" = "1" ] \
    || failed "pu: notes.txt journals: [$(pu_journals notes.txt)], expected [1]"
[ "$(pu_journals .env)" = "0" ] \
    || failed "pu: .env has a journal: [$(pu_journals .env)]"
