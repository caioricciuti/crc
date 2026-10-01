# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh branches-remotes-blame.

# ---- branches, remotes and blame --------------------------------------------
# A throwaway repository with a bare one as origin. The caret's line is
# blamed in the status line; Git > Switch Branch creates a branch from the
# palette; Pull with no upstream says so; Push sets the upstream on origin.
mkdir -p "$T/branchproj"
git init -q -b main "$T/branchproj"
git init -q --bare "$T/branchorigin.git"
printf 'one\ntwo\n' > "$T/branchproj/notes.txt"
git -C "$T/branchproj" add notes.txt
testgit "$T/branchproj" commit -q -m "first notes"
git -C "$T/branchproj" remote add origin "$T/branchorigin.git"
cat > "$T/branch.script" <<SCRIPT
wait 800
wait 600
wait 300
dump $T/branch-blame.out
key 35 cmd p
wait 200
text >switch branch
key 36
wait 200
text feature
dump $T/branch-picker.out
key 36
wait 600
wait 300
dump $T/branch-created.out
key 35 cmd p
wait 200
text >pull
key 36
wait 600
wait 300
dump $T/branch-pull.out
key 35 cmd p
wait 200
text >push
key 36
wait 1500
wait 300
dump $T/branch-push.out
quit
SCRIPT
CRC_SELFTEST="$T/branch.script" "$BIN" "$T/branchproj/notes.txt" 2> "$T/branch.err"
expect "$T/branch-blame.out" branch "main"
grep -q '^blame: Tester, .*: first notes$' "$T/branch-blame.out" \
    || failed "branch-blame.out: $(grep '^blame:' "$T/branch-blame.out")"
expect "$T/branch-picker.out" palette_first "Create branch “feature”"
expect "$T/branch-created.out" branch "feature"
expect "$T/branch-created.out" message "created and switched to feature"
expect "$T/branch-pull.out" message "this branch has no upstream to pull from"
expect "$T/branch-push.out" branch "feature"
grep -q '^message: pushed' "$T/branch-push.out" \
    || failed "branch-push.out: $(grep '^message:' "$T/branch-push.out")"
git -C "$T/branchorigin.git" rev-parse --verify -q refs/heads/feature > /dev/null \
    || failed "branch push: origin has no feature branch"

# ---- renaming and deleting branches -------------------------------------------
# The branch in the status bar opens the list. Git > Rename Branch picks a
# branch, then takes its new name; Git > Delete Branch deletes a merged
# one at once and asks before one with commits nowhere else (answered
# here through the environment, as a test cannot answer a modal).
mkdir -p "$T/editproj"
git init -q -b main "$T/editproj"
printf 'one\n' > "$T/editproj/notes.txt"
git -C "$T/editproj" add notes.txt
testgit "$T/editproj" commit -q -m "first notes"
git -C "$T/editproj" branch old
git -C "$T/editproj" switch -q -c wip
printf 'two\n' >> "$T/editproj/notes.txt"
testgit "$T/editproj" commit -q -am "wip notes"
git -C "$T/editproj" switch -q main
cat > "$T/edit.script" <<SCRIPT
wait 800
wait 600
click @status.branch
wait 400
wait 200
dump $T/edit-status.out
key 53
key 35 cmd p
wait 200
text >rename branch
key 36
wait 400
wait 200
text old
key 36
wait 400
wait 200
text archive
dump $T/edit-rename-to.out
key 36
wait 600
wait 300
dump $T/edit-renamed.out
key 35 cmd p
wait 200
text >delete branch
key 36
wait 400
wait 200
dump $T/edit-delete-list.out
text archive
key 36
wait 600
wait 300
dump $T/edit-deleted.out
key 35 cmd p
wait 200
text >delete branch
key 36
wait 400
wait 200
text wip
key 36
wait 600
wait 300
dump $T/edit-forced.out
quit
SCRIPT
CRC_DELETE_BRANCH_ANSWER=delete CRC_SELFTEST="$T/edit.script" "$BIN" "$T/editproj/notes.txt" 2> "$T/edit.err"
# Both commits can share a second, so either branch may lead the list.
grep -Eq '^palette_first: (main|wip)$' "$T/edit-status.out" \
    || failed "edit-status.out: the status bar's branch did not open the list: $(grep '^palette_first:' "$T/edit-status.out")"
expect "$T/edit-rename-to.out" palette_first "Rename “old” to “archive”"
expect "$T/edit-renamed.out" message "renamed old to archive"
# The list to delete from leaves out the branch you are on.
grep -q '^palette_first: main$' "$T/edit-delete-list.out" \
    && failed "edit-delete-list.out: the current branch is offered for deletion"
expect "$T/edit-deleted.out" message "deleted archive"
grep -q '^crc: delete branch prompt: wip: ' "$T/edit.err" \
    || failed "edit: deleting an unmerged branch did not ask first"
# The prompt it would have shown is the one line this launch may write.
grep -v '^crc: delete branch prompt: ' "$T/edit.err" > "$T/edit.err.rest" || true
mv "$T/edit.err.rest" "$T/edit.err"
expect "$T/edit-forced.out" message "deleted wip"
[ "$(git -C "$T/editproj" for-each-ref --format='%(refname:short)' refs/heads)" = "main" ] \
    || failed "edit: branches left: $(git -C "$T/editproj" for-each-ref --format='%(refname:short)' refs/heads | tr '\n' ' ')"
