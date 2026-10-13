# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh git-changes.

# ---- stepping through and reverting changes -----------------------------------
# notes.txt differs from HEAD in three places when it opens: line 2
# modified, "five" deleted above line 5, line 8 added. Option-F5 steps
# through them and wraps, Shift-Option-F5 goes back; Revert Change puts the
# caret's change back to HEAD in the buffer only, and Cmd-Z brings the edit
# back. Right after a revert the gutter has not diffed again, and the next
# step must still land on the lines as they are now.
mkdir -p "$T/changesproj"
git init -q -b main "$T/changesproj"
printf 'one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n' > "$T/changesproj/notes.txt"
git -C "$T/changesproj" add notes.txt
testgit "$T/changesproj" commit -q -m notes
printf 'one\nTWO\nthree\nfour\nsix\nseven\neight\nnine\n' > "$T/changesproj/notes.txt"
cat > "$T/changes.script" <<SCRIPT
wait 800
wait 600
wait 300
key 96 opt
wait 100
dump $T/changes-1.out
key 96 opt
wait 100
dump $T/changes-2.out
key 96 opt
wait 100
key 96 opt
wait 100
dump $T/changes-wrap.out
key 96 shift,opt
wait 100
dump $T/changes-back.out
key 35 cmd p
wait 200
text >revert change
key 36
wait 100
dump $T/changes-reverted.out
key 96 shift,opt
wait 50
dump $T/changes-after.out
key 35 cmd p
wait 200
text >revert change
key 36
wait 100
dump $T/changes-deletion.out
key 6 cmd z
wait 100
dump $T/changes-undo.out
quit
SCRIPT
CRC_SELFTEST="$T/changes.script" "$BIN" "$T/changesproj/notes.txt" 2> "$T/changes.err"
expect "$T/changes-1.out" cursor "2:1"
expect "$T/changes-1.out" message "change 1 of 3"
expect "$T/changes-2.out" cursor "5:1"
expect "$T/changes-2.out" message "change 2 of 3"
expect "$T/changes-wrap.out" cursor "2:1"
expect "$T/changes-wrap.out" message "change 1 of 3"
expect "$T/changes-back.out" cursor "8:1"
expect "$T/changes-back.out" message "change 3 of 3"
expect "$T/changes-reverted.out" message "change reverted to HEAD"
expect_line "$T/changes-reverted.out" 7 "eight"
expect_line "$T/changes-reverted.out" 8 ""
expect "$T/changes-after.out" cursor "5:1"
expect "$T/changes-after.out" message "change 2 of 2"
expect_line "$T/changes-deletion.out" 5 "five"
expect_line "$T/changes-deletion.out" 6 "six"
expect_line "$T/changes-deletion.out" 2 "TWO"
expect_line "$T/changes-undo.out" 5 "six"
[ "$(cat "$T/changesproj/notes.txt")" = "$(printf 'one\nTWO\nthree\nfour\nsix\nseven\neight\nnine')" ] \
    || failed "changes: Revert Change wrote notes.txt: $(cat "$T/changesproj/notes.txt")"

# A file outside any repository says why there is nothing to step to.
mkdir -p "$T/norepo"
printf 'loose\n' > "$T/norepo/loose.txt"
cat > "$T/norepo.script" <<SCRIPT
wait 800
wait 300
key 96 opt
wait 100
dump $T/norepo.out
quit
SCRIPT
CRC_SELFTEST="$T/norepo.script" "$BIN" "$T/norepo/loose.txt" 2> "$T/norepo.err"
expect "$T/norepo.out" message "this file is not in a Git repository"
