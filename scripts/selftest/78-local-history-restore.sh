# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh local-history.

# ---- restoring a version from File History ------------------------------------
# menu.txt is committed, then edited and saved twice in crc. Opened again
# with "jam" typed and not saved (and the completion it offers closed),
# File History steps to the first save; vs Working Tree marks what it
# changed against the typed text instead of against the commit, and back.
# The working tree has nothing to restore. Restore puts the save into the
# file's tab (Return in the version's tab does nothing; 56 checks that):
# the history closes, the tab holds the save's text, unsaved, the file on
# disk is untouched, the typed text is in local history as "before restore" (reason 7), and
# Cmd-Z brings it back. The next history opens marking against the
# working tree, as the last one did. SELFTEST_FRAMES, when set, is a
# folder the strip's frame is copied to.
mkdir -p "$T/lrproj" "$T/lr"
lrdb="$T/lr/history.db"
git init -q -b main "$T/lrproj"
git -C "$T/lrproj" config core.autocrlf false
printf 'soup\nbread\n' > "$T/lrproj/menu.txt"
git -C "$T/lrproj" add menu.txt
GIT_AUTHOR_DATE=2024-03-01T12:00:00Z GIT_COMMITTER_DATE=2024-03-01T12:00:00Z \
    testgit "$T/lrproj" commit -q -m first

cat > "$T/lr-edit.script" <<SCRIPT
wait 800
text tea
key 36
key 1 cmd s
wait 300
text milk
key 36
key 1 cmd s
wait 300
quit
SCRIPT
CRC_HISTORY_DB="$lrdb" CRC_SELFTEST="$T/lr-edit.script" "$BIN" "$T/lrproj/menu.txt" 2> "$T/lr-edit.err"
[ "$(cat "$T/lrproj/menu.txt")" = "$(printf 'tea\nmilk\nsoup\nbread')" ] \
    || failed "lr: menu.txt was not saved: $(cat "$T/lrproj/menu.txt")"

cat > "$T/lr.script" <<SCRIPT
wait 800
text jam
wait 300
key 53
wait 100
key 16 cmd,opt y
wait 800
wait 300
dump $T/lr-open.out
key 123
wait 300
dump $T/lr-local.out
click @scrub.compare
wait 300
wait 300
dump $T/lr-compare.out
frame $T/lr-strip.bmp
click @scrub.compare
wait 300
wait 300
dump $T/lr-previous.out
click @scrub.compare
wait 300
wait 300
key 119
wait 300
click @scrub.restore
wait 100
dump $T/lr-worktree.out
key 115
wait 300
key 124
wait 300
dump $T/lr-before.out
click @scrub.restore
wait 300
dump $T/lr-restored.out
key 6 cmd z
wait 300
dump $T/lr-undone.out
key 16 cmd,opt y
wait 800
wait 300
dump $T/lr-reopened.out
quit
SCRIPT
CRC_HISTORY_DB="$lrdb" CRC_DISCARD_ANSWER=dont-save \
    CRC_SELFTEST="$T/lr.script" "$BIN" "$T/lrproj/menu.txt" 2> "$T/lr.err"
c1=$(git -C "$T/lrproj" rev-parse --short=7 HEAD)
expect "$T/lr-open.out" scrub_filter "all 3/3 $c1,local:saved,local:saved"
case "$(grep -m1 '^scrub: ' "$T/lr-local.out")" in
    "scrub: 2/3@local:saved "*"first=tea") ;;
    *) failed "lr: the first save: $(grep -m1 '^scrub: ' "$T/lr-local.out")" ;;
esac
# Against the commit: "tea" added.
expect "$T/lr-local.out" gutter "1A inactive"
expect "$T/lr-local.out" scrub_compare "previous marks=true restore=ready"
# Against the typed text: "jamtea" and "milk" are "tea" here, one
# change.
expect "$T/lr-compare.out" scrub_compare "working tree marks=true restore=ready"
expect "$T/lr-compare.out" gutter "1M inactive"
expect "$T/lr-compare.out" message "marking changes against the working tree"
expect "$T/lr-previous.out" gutter "1A inactive"
expect "$T/lr-previous.out" scrub_compare "previous marks=true restore=ready"
expect "$T/lr-worktree.out" message "this is the working tree: there is nothing to restore"
expect "$T/lr-worktree.out" tabs "menu.txt | menu.txt @ working tree"
case "$(grep -m1 '^scrub: ' "$T/lr-before.out")" in
    "scrub: 2/3@local:saved "*"first=tea") ;;
    *) failed "lr: back at the first save: $(grep -m1 '^scrub: ' "$T/lr-before.out")" ;;
esac
# Restored: the history closed, the save's text in the file's tab,
# unsaved, and the file on disk as it was.
expect "$T/lr-restored.out" scrub "closed"
expect "$T/lr-restored.out" tabs "menu.txt"
expect "$T/lr-restored.out" dirty "true"
expect_line "$T/lr-restored.out" 1 "tea"
expect_line "$T/lr-restored.out" 2 "soup"
expect_line "$T/lr-restored.out" 3 "bread"
case "$(grep -m1 '^message: ' "$T/lr-restored.out")" in
    "message: restored menu.txt @ saved "[0-2][0-9]:[0-5][0-9]", not saved: Undo takes it back") ;;
    *) failed "lr: restored says: $(grep -m1 '^message: ' "$T/lr-restored.out")" ;;
esac
[ "$(cat "$T/lrproj/menu.txt")" = "$(printf 'tea\nmilk\nsoup\nbread')" ] \
    || failed "lr: menu.txt was written: $(cat "$T/lrproj/menu.txt")"
# Undone: the typed text back, still unsaved.
expect_line "$T/lr-undone.out" 1 "jamtea"
expect_line "$T/lr-undone.out" 2 "milk"
expect "$T/lr-undone.out" dirty "true"
expect "$T/lr-reopened.out" scrub_compare "working tree marks=true restore=ready"
# The typed text, 23 bytes, kept as before restore.
lr_rows=$(/usr/bin/sqlite3 "$lrdb" "SELECT group_concat(row, ' ') FROM (
    SELECT s.reason || ':' || b.size AS row FROM snapshots s
    JOIN files f ON f.id = s.file JOIN blobs b ON b.id = s.blob
    WHERE f.path LIKE '%/lrproj/menu.txt' ORDER BY s.id)" 2>&1)
case " $lr_rows " in
    *" 7:23 "*) ;;
    *) failed "lr: no before-restore row of the typed text: [$lr_rows]" ;;
esac
[ -z "${SELFTEST_FRAMES:-}" ] || cp -f "$T/lr-strip.bmp" "$SELFTEST_FRAMES/"
