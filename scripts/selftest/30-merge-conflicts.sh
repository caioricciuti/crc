# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh merge-conflicts.

# ---- merge conflicts ----------------------------------------------------------
# A merge in the terminal leaves two conflicts in plan.txt, zdiff3 style.
# The strip above the text counts them and Git's state reads MERGING. Next
# goes to the first; its inline button takes the incoming side; the columns
# take both sides of the second; undo brings it back and the palette's
# Accept Base settles it; Mark Resolved saves and stages the file.
mkdir -p "$T/mergeproj"
mgit init -q -b main
printf 'garden plan\nwater: morning\nshade: none\nsoil: loam\npath: gravel\nfence: wood\nbeds: 3\nend\n' > "$T/mergeproj/plan.txt"
mgit add plan.txt
mgit commit -q -m base
mgit switch -q -c topic
printf 'garden plan\nwater: evening\nshade: none\nsoil: loam\npath: gravel\nfence: wood\nbeds: 4\nend\n' > "$T/mergeproj/plan.txt"
mgit commit -q -am topic
mgit switch -q main
printf 'garden plan\nwater: noon\nshade: none\nsoil: loam\npath: gravel\nfence: wood\nbeds: 5\nend\n' > "$T/mergeproj/plan.txt"
mgit commit -q -am main
mgit merge -q topic > /dev/null 2>&1 && { echo "FAIL merge: the fixture did not conflict"; fail=1; }
cat > "$T/merge.script" <<SCRIPT
wait 800
wait 600
wait 300
dump $T/merge-open.out
click @conflict.next
wait 100
dump $T/merge-next.out
click @conflict.incoming.0
wait 200
dump $T/merge-took.out
click @conflict.side
wait 200
dump $T/merge-side.out
click @conflict.both.0
wait 200
dump $T/merge-both.out
key 6 cmd z
wait 200
dump $T/merge-undo.out
key 35 cmd p
wait 200
text >accept base
key 36
wait 200
dump $T/merge-base.out
click @conflict.resolve
wait 800
wait 600
wait 300
dump $T/merge-resolved.out
quit
SCRIPT
CRC_SELFTEST="$T/merge.script" "$BIN" "$T/mergeproj/plan.txt" 2> "$T/merge.err"
expect "$T/merge-open.out" conflicts "2 side=false unmerged=true resolvable=false scroll=0"
expect "$T/merge-open.out" git_conflicts "1"
expect "$T/merge-open.out" branch "main · MERGING"
expect "$T/merge-next.out" cursor "2:1"
expect "$T/merge-next.out" message "conflict 1 of 2"
grep -q '^conflicts: 1 side=false unmerged=true resolvable=false ' "$T/merge-took.out" \
    || failed "merge-took.out: $(grep '^conflicts:' "$T/merge-took.out")"
expect_line "$T/merge-took.out" 2 "water: evening"
expect_line "$T/merge-took.out" 7 "<<<<<<< HEAD"
grep -q '^conflicts: 1 side=true ' "$T/merge-side.out" \
    || failed "merge-side.out: $(grep '^conflicts:' "$T/merge-side.out")"
grep -q '^conflicts: 0 side=false unmerged=true resolvable=true ' "$T/merge-both.out" \
    || failed "merge-both.out: $(grep '^conflicts:' "$T/merge-both.out")"
expect_line "$T/merge-both.out" 7 "beds: 5"
expect_line "$T/merge-both.out" 8 "beds: 4"
grep -q '^conflicts: 1 side=true ' "$T/merge-undo.out" \
    || failed "merge-undo.out: $(grep '^conflicts:' "$T/merge-undo.out")"
expect_line "$T/merge-undo.out" 2 "water: evening"
expect_line "$T/merge-base.out" 7 "beds: 3"
expect_line "$T/merge-base.out" 8 "end"
expect "$T/merge-resolved.out" conflicts "none"
expect "$T/merge-resolved.out" git_conflicts "0"
expect "$T/merge-resolved.out" message "marked plan.txt resolved"
[ "$(cat "$T/mergeproj/plan.txt")" = "$(printf 'garden plan\nwater: evening\nshade: none\nsoil: loam\npath: gravel\nfence: wood\nbeds: 3\nend')" ] \
    || failed "merge: plan.txt on disk is"; cat "$T/mergeproj/plan.txt"
[ -z "$(mgit ls-files -u)" ] || failed "merge: plan.txt is still unmerged"
[ "$(mgit diff --cached --name-only)" = "plan.txt" ] \
    || failed "merge: plan.txt is not staged"

# ---- giving up a merge ----------------------------------------------------------
# Git > Abort Merge or Rebase asks first (answered here through the
# environment), then puts the branch and file back as they were before
# the merge: MERGING leaves the status bar and the markers leave the file.
mkdir -p "$T/abortproj"
testgit "$T/abortproj" init -q -b main
printf 'garden plan\nbeds: 3\n' > "$T/abortproj/plan.txt"
testgit "$T/abortproj" add plan.txt
testgit "$T/abortproj" commit -q -m base
testgit "$T/abortproj" switch -q -c topic
printf 'garden plan\nbeds: 4\n' > "$T/abortproj/plan.txt"
testgit "$T/abortproj" commit -q -am topic
testgit "$T/abortproj" switch -q main
printf 'garden plan\nbeds: 5\n' > "$T/abortproj/plan.txt"
testgit "$T/abortproj" commit -q -am main
testgit "$T/abortproj" merge -q topic > /dev/null 2>&1 && failed "abort: the fixture did not conflict"
cat > "$T/abort.script" <<SCRIPT
wait 800
wait 600
dump $T/abort-merging.out
key 35 cmd p
wait 200
text >abort merge
key 36
wait 600
wait 300
dump $T/abort-done.out
quit
SCRIPT
CRC_ABORT_ANSWER=abort CRC_SELFTEST="$T/abort.script" "$BIN" "$T/abortproj/plan.txt" 2> "$T/abort.err"
expect "$T/abort-merging.out" branch "main · MERGING"
expect "$T/abort-done.out" branch "main"
expect "$T/abort-done.out" message "merge aborted"
[ "$(cat "$T/abortproj/plan.txt")" = "$(printf 'garden plan\nbeds: 5')" ] \
    || failed "abort: plan.txt is not back to main's: $(cat "$T/abortproj/plan.txt")"
grep -q '^crc: abort prompt: merge: ' "$T/abort.err" \
    || failed "abort: giving up the merge did not ask first"
grep -v '^crc: abort prompt: ' "$T/abort.err" > "$T/abort.err.rest" || true
mv "$T/abort.err.rest" "$T/abort.err"
