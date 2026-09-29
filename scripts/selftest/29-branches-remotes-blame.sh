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
