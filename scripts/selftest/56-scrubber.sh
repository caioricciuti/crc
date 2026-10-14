# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh scrubber.

# ---- stepping through a file's history ------------------------------------------
# journal.txt has five versions: two as notes.txt, a rename, one stored
# with CRLF line breaks, and the newest; the working tree adds a line on
# top. Cmd-Option-Y opens the newest commit in a read-only tab beside the
# file, the caret on the line it was on in the file. Left steps older and
# the caret follows its line, Home goes to the oldest version and End to
# the working tree. Typing and Cmd-S change nothing anywhere, and Escape
# goes back to the file as it was.
mkdir -p "$T/scrubproj"
git init -q -b main "$T/scrubproj"
git -C "$T/scrubproj" config core.autocrlf false
printf 'alpha\nbeta\ngamma\n' > "$T/scrubproj/notes.txt"
git -C "$T/scrubproj" add notes.txt
testgit "$T/scrubproj" commit -q -m one
printf 'alpha\nbeta\ngamma\ndelta\n' > "$T/scrubproj/notes.txt"
testgit "$T/scrubproj" commit -q -am two
git -C "$T/scrubproj" mv notes.txt journal.txt
testgit "$T/scrubproj" commit -q -m renamed
printf 'zero\r\nalpha\r\nbeta\r\ngamma\r\ndelta\r\n' > "$T/scrubproj/journal.txt"
testgit "$T/scrubproj" commit -q -am crlf
printf 'zero\nalpha\nBETA\ngamma\ndelta\nepsilon\n' > "$T/scrubproj/journal.txt"
testgit "$T/scrubproj" commit -q -am five
printf 'zero\nalpha\nBETA\ngamma\ndelta\nepsilon\nzeta\n' > "$T/scrubproj/journal.txt"
printf 'loose\n' > "$T/scrubproj/untracked.txt"
scrub_hash() { git -C "$T/scrubproj" rev-parse --short=7 "$1"; }
cat > "$T/scrub.script" <<SCRIPT
wait 800
wait 600
wait 300
key 125
key 125
key 125
key 16 cmd,opt y
wait 800
dump $T/scrub-open.out
key 123
wait 300
dump $T/scrub-older.out
key 123
wait 300
dump $T/scrub-rename.out
key 115
wait 300
dump $T/scrub-oldest.out
key 124
wait 300
dump $T/scrub-newer.out
key 119
wait 300
dump $T/scrub-end.out
key 123
wait 300
text xy
key 51
key 36
key 6 cmd z
wait 100
dump $T/scrub-typed.out
key 1 cmd s
wait 100
dump $T/scrub-save.out
key 1 cmd,shift s
wait 100
key 53
wait 100
dump $T/scrub-closed.out
quit
SCRIPT
CRC_SELFTEST="$T/scrub.script" "$BIN" "$T/scrubproj/journal.txt" 2> "$T/scrub.err"
# What is loaded beyond the versions shown depends on when the display
# link last polled, so only the start and the end of the line are fixed.
case "$(grep -m1 '^scrub: ' "$T/scrub-open.out")" in
    "scrub: 5/5@$(scrub_hash HEAD) loaded="*" pending=None first=zero") ;;
    *) failed "scrubber: open: $(grep -m1 '^scrub: ' "$T/scrub-open.out")" ;;
esac
expect "$T/scrub-open.out" tabs "journal.txt | journal.txt @ $(scrub_hash HEAD)"
expect "$T/scrub-open.out" active "1"
expect "$T/scrub-open.out" cursor "4:1"
expect "$T/scrub-open.out" message "$(scrub_hash HEAD) Tester, just now: five (5 of 5)"
# What the commit changed; nothing for Next Change to act on.
expect "$T/scrub-open.out" gutter "3M 6A inactive"
expect "$T/scrub-open.out" dirty "false"
expect_line "$T/scrub-open.out" 6 "epsilon"
expect_line "$T/scrub-open.out" 7 ""
case "$(grep -m1 '^scrub: ' "$T/scrub-older.out")" in
    "scrub: 4/5@$(scrub_hash HEAD~1) "*) ;;
    *) failed "scrubber: one step older: $(grep -m1 '^scrub: ' "$T/scrub-older.out")" ;;
esac
# The CRLF version, with the line breaks the buffer keeps.
expect_line "$T/scrub-older.out" 1 "zero"
expect_line "$T/scrub-older.out" 3 "beta"
expect "$T/scrub-older.out" cursor "4:1"
expect "$T/scrub-older.out" gutter "1A inactive"
[ "$(sed -n '/^text:$/,$p' "$T/scrub-older.out" | tail -n +2)" = "$(git -C "$T/scrubproj" show HEAD~1:journal.txt | tr -d '\r')" ] \
    || failed "scrubber: the CRLF version is not its blob"
case "$(grep -m1 '^scrub: ' "$T/scrub-rename.out")" in
    "scrub: 3/5@$(scrub_hash HEAD~2) "*) ;;
    *) failed "scrubber: the rename: $(grep -m1 '^scrub: ' "$T/scrub-rename.out")" ;;
esac
[ "$(sed -n '/^text:$/,$p' "$T/scrub-rename.out" | tail -n +2)" = "$(git -C "$T/scrubproj" show HEAD~2:journal.txt)" ] \
    || failed "scrubber: the renamed version is not its blob"
# gamma: line 4 with zero above it, line 3 before zero was added.
expect "$T/scrub-rename.out" cursor "3:1"
expect "$T/scrub-rename.out" gutter " inactive"
expect "$T/scrub-rename.out" tabs "journal.txt | journal.txt @ $(scrub_hash HEAD~2)"
case "$(grep -m1 '^scrub: ' "$T/scrub-oldest.out")" in
    "scrub: 1/5@$(scrub_hash HEAD~4) "*"first=alpha") ;;
    *) failed "scrubber: Home: $(grep -m1 '^scrub: ' "$T/scrub-oldest.out")" ;;
esac
expect "$T/scrub-oldest.out" cursor "3:1"
# The oldest version has no parent read: no marks, not every line added.
expect "$T/scrub-oldest.out" gutter " inactive"
expect "$T/scrub-oldest.out" message "$(scrub_hash HEAD~4) Tester, just now: one (1 of 5)"
expect "$T/scrub-newer.out" gutter "4A inactive"
expect "$T/scrub-newer.out" cursor "3:1"
case "$(grep -m1 '^scrub: ' "$T/scrub-end.out")" in
    "scrub: worktree/5 "*) ;;
    *) failed "scrubber: End: $(grep -m1 '^scrub: ' "$T/scrub-end.out")" ;;
esac
expect "$T/scrub-end.out" tabs "journal.txt | journal.txt @ working tree"
expect "$T/scrub-end.out" message "working tree, after 5 commits"
expect_line "$T/scrub-end.out" 7 "zeta"
expect "$T/scrub-typed.out" dirty "false"
case "$(grep -m1 '^scrub: ' "$T/scrub-typed.out")" in
    "scrub: 5/5@$(scrub_hash HEAD) "*) ;;
    *) failed "scrubber: after typing: $(grep -m1 '^scrub: ' "$T/scrub-typed.out")" ;;
esac
[ "$(sed -n '/^text:$/,$p' "$T/scrub-typed.out" | tail -n +2)" = "$(git -C "$T/scrubproj" show HEAD:journal.txt)" ] \
    || failed "scrubber: typing changed a version"
expect "$T/scrub-save.out" message "a version from history is not saved"
expect "$T/scrub-save.out" dirty "false"
expect "$T/scrub-closed.out" scrub "closed"
expect "$T/scrub-closed.out" tabs "journal.txt"
expect "$T/scrub-closed.out" cursor "4:1"
expect "$T/scrub-closed.out" dirty "false"
expect_line "$T/scrub-closed.out" 7 "zeta"
[ "$(cat "$T/scrubproj/journal.txt")" = "$(printf 'zero\nalpha\nBETA\ngamma\ndelta\nepsilon\nzeta')" ] \
    || failed "scrubber: journal.txt was written: $(cat "$T/scrubproj/journal.txt")"
[ "$(git -C "$T/scrubproj" status --porcelain | tr '\n' ' ')" = " M journal.txt ?? untracked.txt " ] \
    || failed "scrubber: the repository changed: $(git -C "$T/scrubproj" status --porcelain)"

# A file Git does not track has no history, and says so.
cat > "$T/scrub-untracked.script" <<SCRIPT
wait 800
wait 300
key 16 cmd,opt y
wait 800
dump $T/scrub-untracked.out
quit
SCRIPT
CRC_SELFTEST="$T/scrub-untracked.script" "$BIN" "$T/scrubproj/untracked.txt" 2> "$T/scrub-untracked.err"
expect "$T/scrub-untracked.out" message "untracked.txt has no history: Git does not track it"
expect "$T/scrub-untracked.out" scrub "closed"
expect "$T/scrub-untracked.out" tabs "untracked.txt"
