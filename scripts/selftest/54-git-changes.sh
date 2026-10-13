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

# ---- peeking at a change and staging one ---------------------------------------
# A click on line 2's mark opens the peek at it, with the caret there;
# Escape closes it. The peek's Stage puts that change alone in the index,
# from the buffer; Option-F5 with the peek open moves it to the next change,
# and Stage Change from the palette stages the caret's change. The file on
# disk is never written. Staging writes the index, and the watcher has the
# gutter read HEAD again: the marks stay drawn all the while, sampled every
# few frames, and Next Change works at once, without waiting for it. Last,
# HEAD moves to another branch behind the editor, and the marks follow it.
mkdir -p "$T/peekproj"
git init -q -b main "$T/peekproj"
printf 'one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n' > "$T/peekproj/notes.txt"
git -C "$T/peekproj" add notes.txt
testgit "$T/peekproj" commit -q -m notes
git -C "$T/peekproj" checkout -q -b other
printf 'one\nTWO\nthree\nfour\nsix\nseven\neight\n' > "$T/peekproj/notes.txt"
testgit "$T/peekproj" commit -q -am other
git -C "$T/peekproj" checkout -q main
printf 'one\nTWO\nthree\nfour\nsix\nseven\neight\nnine\n' > "$T/peekproj/notes.txt"
cat > "$T/peek.script" <<SCRIPT
wait 800
wait 600
wait 300
click @change_bar.2
wait 100
dump $T/peek-open.out
frame $T/peek-open.bmp
key 53
wait 100
dump $T/peek-closed.out
click @change_bar.2
wait 100
click @peek.stage
wait 300
dump $T/peek-staged.out
SCRIPT
for n in $(seq 1 30); do
    printf 'wait 25\ndump %s\n' "$T/peek-settle-$n.out"
done >> "$T/peek.script"
cat >> "$T/peek.script" <<SCRIPT
key 96 opt
wait 100
dump $T/peek-moved.out
key 96 opt
wait 100
key 35 cmd p
wait 200
text >stage change
key 36
wait 1500
dump $T/peek-staged-two.out
write $T/peekproj/.git/HEAD ref: refs/heads/other
SCRIPT
for n in $(seq 1 30); do
    printf 'wait 25\ndump %s\n' "$T/peek-switch-$n.out"
done >> "$T/peek.script"
cat >> "$T/peek.script" <<SCRIPT
key 96 opt
wait 100
dump $T/peek-branch.out
quit
SCRIPT
CRC_SELFTEST="$T/peek.script" "$BIN" "$T/peekproj/notes.txt" 2> "$T/peek.err"
expect "$T/peek-open.out" peek "Changed 1 line, 1 in HEAD 2..3 drawn=true"
expect "$T/peek-open.out" cursor "2:1"
expect "$T/peek-closed.out" peek "closed"
for n in $(seq 1 30); do
    expect "$T/peek-settle-$n.out" gutter "2M 5R 8A"
    case "$(grep -m1 '^gutter: ' "$T/peek-switch-$n.out")" in
        "gutter: 2M 5R 8A" | "gutter: 8A") ;;
        *) failed "peek: the gutter while HEAD moved: $(grep -m1 '^gutter: ' "$T/peek-switch-$n.out")" ;;
    esac
done
expect "$T/peek-staged.out" message "staged the change at line 2"
expect "$T/peek-staged.out" peek "Changed 1 line, 1 in HEAD 2..3 drawn=true"
expect "$T/peek-moved.out" peek "Removed 1 line 5..5 drawn=true"
expect "$T/peek-moved.out" cursor "5:1"
expect "$T/peek-staged-two.out" message "staged the change at line 8"
expect "$T/peek-branch.out" gutter "8A"
expect "$T/peek-branch.out" message "change 1 of 1"
[ "$(git -C "$T/peekproj" show :notes.txt)" = "$(printf 'one\nTWO\nthree\nfour\nfive\nsix\nseven\neight\nnine')" ] \
    || failed "peek: the index has: $(git -C "$T/peekproj" show :notes.txt)"
[ "$(cat "$T/peekproj/notes.txt")" = "$(printf 'one\nTWO\nthree\nfour\nsix\nseven\neight\nnine')" ] \
    || failed "peek: staging wrote notes.txt: $(cat "$T/peekproj/notes.txt")"

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
