# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh local-history-commands.

# ---- local history commands: recover a deleted file, clear history ------------
# gone.txt, keep.txt and other.txt are edited and saved in crc, and gone.txt
# is deleted. Recover Deleted File lists it in the palette, by name with
# its folder; picked, its last version opens at its old path, unsaved, and
# Cmd-S writes the file back. sub/deep/notes.md goes with its folders:
# recovered, Cmd-S makes them again. Clear History for This File asks first: with
# no answer nothing goes, and the prompt it would have shown is said on
# stderr. Answered, from File History's tab of keep.txt, the history
# closes at once and only keep.txt's versions and undo go. Clear All Local
# History, with File History open on other.txt, closes it and empties
# every table. No test instance shows a modal.
mkdir -p "$T/lhcproj" "$T/lhc"
lhcdb="$T/lhc/history.db"
for lhc_file in gone keep other; do
    printf '%s\n' "$lhc_file" > "$T/lhcproj/$lhc_file.txt"
done
mkdir -p "$T/lhcproj/sub/deep"
printf 'deep\n' > "$T/lhcproj/sub/deep/notes.md"

# lhc_run <name> <path> [VAR=value...]: crc on <path> with $T/lhc-<name>.script.
lhc_run() {
    local run=$1 path=$2
    shift 2
    env CRC_HISTORY_DB="$lhcdb" CRC_DISCARD_ANSWER=dont-save "$@" \
        CRC_SELFTEST="$T/lhc-$run.script" "$BIN" "$path" 2> "$T/lhc-$run.err"
}
# lhc_count <sql>: one number from the history.
lhc_count() {
    /usr/bin/sqlite3 "$lhcdb" "$1" 2>&1
}
# lhc_rows <file name>: versions kept of it.
lhc_rows() {
    lhc_count "SELECT count(*) FROM snapshots s JOIN files f ON f.id = s.file
        WHERE f.path LIKE '%/lhcproj/$1'"
}
# lhc_undo <file name>: undo journals kept for it.
lhc_undo() {
    lhc_count "SELECT count(*) FROM undo u JOIN files f ON f.id = u.file
        WHERE f.path LIKE '%/lhcproj/$1'"
}
# lhc_prompted <name>: the run asked to clear; the line is taken out, as
# the one this launch may write.
lhc_prompted() {
    grep -q '^crc: clear history prompt: ' "$T/lhc-$1.err" \
        || failed "lhc-$1: the clear did not ask first"
    grep -v '^crc: clear history prompt: ' "$T/lhc-$1.err" > "$T/lhc-$1.err.rest" || true
    mv "$T/lhc-$1.err.rest" "$T/lhc-$1.err"
}

cat > "$T/lhc-edit.script" <<SCRIPT
wait 800
key 35 cmd p
text notes.md
key 36
wait 300
text e
key 1 cmd s
wait 200
key 35 cmd p
text gone.txt
key 36
wait 300
text a
key 1 cmd s
wait 200
text b
key 1 cmd s
wait 200
key 35 cmd p
text keep.txt
key 36
wait 300
text c
key 1 cmd s
wait 200
key 35 cmd p
text other.txt
key 36
wait 300
text d
key 1 cmd s
wait 300
quit
SCRIPT
lhc_run edit "$T/lhcproj"
[ "$(cat "$T/lhcproj/gone.txt")" = "abgone" ] \
    || failed "lhc: gone.txt was not saved: $(cat "$T/lhcproj/gone.txt")"
for lhc_file in gone.txt keep.txt other.txt; do
    [ "$(lhc_rows $lhc_file)" -gt 0 ] 2>/dev/null \
        || failed "lhc: no versions of $lhc_file: [$(lhc_rows $lhc_file)]"
done
[ "$(lhc_undo keep.txt)" = "1" ] \
    || failed "lhc: keep.txt journals: [$(lhc_undo keep.txt)], expected [1]"
[ "$(cat "$T/lhcproj/sub/deep/notes.md")" = "edeep" ] \
    || failed "lhc: notes.md was not saved: $(cat "$T/lhcproj/sub/deep/notes.md")"
rm -f "$T/lhcproj/gone.txt"
rm -rf "$T/lhcproj/sub"

cat > "$T/lhc-recover.script" <<SCRIPT
wait 800
key 35 cmd p
text >recover deleted
wait 100
dump $T/lhc-command.out
key 36
wait 300
wait 200
dump $T/lhc-list.out
key 36
wait 300
wait 200
dump $T/lhc-recovered.out
key 1 cmd s
wait 300
dump $T/lhc-saved.out
quit
SCRIPT
lhc_run recover "$T/lhcproj/keep.txt"
expect "$T/lhc-command.out" palette_first "Recover Deleted File…"
expect "$T/lhc-list.out" palette_first "gone.txt"
case "$(grep -m1 '^tabs: ' "$T/lhc-recovered.out")" in
    *gone.txt*) ;;
    *) failed "lhc: gone.txt did not open: $(grep -m1 '^tabs: ' "$T/lhc-recovered.out")" ;;
esac
expect "$T/lhc-recovered.out" dirty "true"
expect_line "$T/lhc-recovered.out" 1 "abgone"
expect "$T/lhc-recovered.out" message "recovered gone.txt from local history, not saved: Save writes it back"
expect "$T/lhc-saved.out" dirty "false"
[ "$(cat "$T/lhcproj/gone.txt" 2>&1)" = "abgone" ] \
    || failed "lhc: gone.txt was not written back: $(cat "$T/lhcproj/gone.txt" 2>&1)"

# Its folders gone too: picked by name, Cmd-S makes them again.
cat > "$T/lhc-folder.script" <<SCRIPT
wait 800
key 35 cmd p
text >recover deleted
key 36
wait 300
wait 200
text notes
wait 100
dump $T/lhc-folder-list.out
key 36
wait 300
wait 200
dump $T/lhc-folder.out
key 1 cmd s
wait 300
dump $T/lhc-folder-saved.out
quit
SCRIPT
lhc_run folder "$T/lhcproj/keep.txt"
expect "$T/lhc-folder-list.out" palette_first "notes.md"
expect "$T/lhc-folder.out" dirty "true"
expect_line "$T/lhc-folder.out" 1 "edeep"
expect "$T/lhc-folder-saved.out" dirty "false"
[ "$(cat "$T/lhcproj/sub/deep/notes.md" 2>&1)" = "edeep" ] \
    || failed "lhc: notes.md was not written back: $(cat "$T/lhcproj/sub/deep/notes.md" 2>&1)"

# Nothing deleted any more: the palette does not open, the status says so.
cat > "$T/lhc-none.script" <<SCRIPT
wait 800
key 35 cmd p
text >recover deleted
key 36
wait 300
wait 200
dump $T/lhc-none.out
quit
SCRIPT
lhc_run none "$T/lhcproj/keep.txt"
expect "$T/lhc-none.out" message "no deleted files in local history"
expect "$T/lhc-none.out" palette_query ""

keep_rows=$(lhc_rows keep.txt)
cat > "$T/lhc-cancel.script" <<SCRIPT
wait 800
key 35 cmd p
text >clear history for this
key 36
wait 300
dump $T/lhc-cancel.out
quit
SCRIPT
lhc_run cancel "$T/lhcproj/keep.txt"
lhc_prompted cancel
[ "$(lhc_rows keep.txt)" = "$keep_rows" ] \
    || failed "lhc: unanswered, keep.txt rows went from $keep_rows to [$(lhc_rows keep.txt)]"
[ "$(lhc_undo keep.txt)" = "1" ] \
    || failed "lhc: unanswered, keep.txt journals: [$(lhc_undo keep.txt)]"

cat > "$T/lhc-clear.script" <<SCRIPT
wait 800
key 16 cmd,opt y
wait 800
wait 300
dump $T/lhc-history.out
key 35 cmd p
text >clear history for this
key 36
wait 100
dump $T/lhc-cleared.out
quit
SCRIPT
lhc_run clear "$T/lhcproj/keep.txt" CRC_CLEAR_HISTORY_ANSWER=clear
lhc_prompted clear
case "$(grep -m1 '^scrub: ' "$T/lhc-history.out")" in
    "scrub: closed"|"scrub: listing"*) failed "lhc: File History did not open: $(grep -m1 '^scrub: ' "$T/lhc-history.out")" ;;
esac
expect "$T/lhc-cleared.out" scrub "closed"
expect "$T/lhc-cleared.out" tabs "keep.txt"
expect "$T/lhc-cleared.out" message "cleared the local history of keep.txt"
[ "$(lhc_rows keep.txt)" = "0" ] \
    || failed "lhc: keep.txt rows left: [$(lhc_rows keep.txt)]"
[ "$(lhc_undo keep.txt)" = "0" ] \
    || failed "lhc: keep.txt journals left: [$(lhc_undo keep.txt)]"
[ "$(lhc_rows other.txt)" -gt 0 ] 2>/dev/null \
    || failed "lhc: other.txt went too: [$(lhc_rows other.txt)]"
[ "$(lhc_rows gone.txt)" -gt 0 ] 2>/dev/null \
    || failed "lhc: gone.txt went too: [$(lhc_rows gone.txt)]"

cat > "$T/lhc-all.script" <<SCRIPT
wait 800
key 16 cmd,opt y
wait 800
wait 300
dump $T/lhc-all-history.out
key 35 cmd p
text >clear all local
key 36
wait 100
dump $T/lhc-all.out
quit
SCRIPT
lhc_run all "$T/lhcproj/other.txt" CRC_CLEAR_HISTORY_ANSWER=clear
lhc_prompted all
case "$(grep -m1 '^scrub: ' "$T/lhc-all-history.out")" in
    "scrub: closed"|"scrub: listing"*) failed "lhc: File History did not open: $(grep -m1 '^scrub: ' "$T/lhc-all-history.out")" ;;
esac
expect "$T/lhc-all.out" scrub "closed"
expect "$T/lhc-all.out" message "cleared all local history"
for table in files blobs snapshots undo; do
    [ "$(lhc_count "SELECT count(*) FROM $table")" = "0" ] \
        || failed "lhc: $table left after clearing all: [$(lhc_count "SELECT count(*) FROM $table")]"
done
