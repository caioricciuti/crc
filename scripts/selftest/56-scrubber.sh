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
# Fixed dates a day apart: the log's order, and the age each version is
# said to have, do not hang on how fast the commits were made.
scrub_commit() {
    local when="2024-03-0$1T12:00:00Z"
    shift
    GIT_AUTHOR_DATE="$when" GIT_COMMITTER_DATE="$when" testgit "$T/scrubproj" commit -q "$@"
}
printf 'alpha\nbeta\ngamma\n' > "$T/scrubproj/notes.txt"
git -C "$T/scrubproj" add notes.txt
scrub_commit 1 -m one
printf 'alpha\nbeta\ngamma\ndelta\n' > "$T/scrubproj/notes.txt"
scrub_commit 2 -am two
git -C "$T/scrubproj" mv notes.txt journal.txt
scrub_commit 3 -m renamed
printf 'zero\r\nalpha\r\nbeta\r\ngamma\r\ndelta\r\n' > "$T/scrubproj/journal.txt"
scrub_commit 4 -am crlf
printf 'zero\nalpha\nBETA\ngamma\ndelta\nepsilon\n' > "$T/scrubproj/journal.txt"
scrub_commit 5 -am five
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
dump $T/scrub-fading.out
wait 300
dump $T/scrub-older.out
key 123
fade 40
dump $T/scrub-held.out
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
# The age moves on with the calendar; the rest of the line does not.
case "$(grep -m1 '^message: ' "$T/scrub-open.out")" in
    "message: $(scrub_hash HEAD) Tester, "*" ago: five (5 of 5)") ;;
    *) failed "scrubber: open: $(grep -m1 '^message: ' "$T/scrub-open.out")" ;;
esac
# The strip above the version says the same, and offers both steps.
case "$(grep -m1 '^scrub_strip: ' "$T/scrub-open.out")" in
    "scrub_strip: $(scrub_hash HEAD)   Tester, "*" ago   five | 5 of 5 | older=true newer=true") ;;
    *) failed "scrubber: strip: $(grep -m1 '^scrub_strip: ' "$T/scrub-open.out")" ;;
esac
# What the commit changed; nothing for Next Change to act on.
expect "$T/scrub-open.out" gutter "3M 6A inactive"
expect "$T/scrub-open.out" dirty "false"
expect_line "$T/scrub-open.out" 6 "epsilon"
expect_line "$T/scrub-open.out" 7 ""
# The step marks what it changed for a moment: BETA's line, which was
# beta, and epsilon, which this version does not have yet. Gone after
# 300 ms, and the display link with it.
case "$(grep -m1 '^scrub_fade: ' "$T/scrub-fading.out")" in
    "scrub_fade: t="*"ms, tints=1 ghosts=1") ;;
    *) failed "scrubber: fade: $(grep -m1 '^scrub_fade: ' "$T/scrub-fading.out")" ;;
esac
expect "$T/scrub-open.out" scrub_fade "none"
expect "$T/scrub-older.out" scrub_fade "none"
# Held 40 ms in by the script: zero, which the renamed version lacks.
expect "$T/scrub-held.out" scrub_fade "t=40ms, tints=0 ghosts=1"
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
case "$(grep -m1 '^message: ' "$T/scrub-oldest.out")" in
    "message: $(scrub_hash HEAD~4) Tester, "*" ago: one (1 of 5)") ;;
    *) failed "scrubber: Home: $(grep -m1 '^message: ' "$T/scrub-oldest.out")" ;;
esac
case "$(grep -m1 '^scrub_strip: ' "$T/scrub-oldest.out")" in
    "scrub_strip: $(scrub_hash HEAD~4)   Tester, "*" ago   one | 1 of 5 | older=false newer=true") ;;
    *) failed "scrubber: strip at the oldest: $(grep -m1 '^scrub_strip: ' "$T/scrub-oldest.out")" ;;
esac
expect "$T/scrub-newer.out" gutter "4A inactive"
expect "$T/scrub-newer.out" cursor "3:1"
case "$(grep -m1 '^scrub: ' "$T/scrub-end.out")" in
    "scrub: worktree/5 "*) ;;
    *) failed "scrubber: End: $(grep -m1 '^scrub: ' "$T/scrub-end.out")" ;;
esac
expect "$T/scrub-end.out" tabs "journal.txt | journal.txt @ working tree"
expect "$T/scrub-end.out" message "working tree, after 5 commits"
expect "$T/scrub-end.out" scrub_strip "journal.txt as open in its tab, after 5 commits | working tree | older=true newer=false"
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
expect "$T/scrub-closed.out" scrub_strip "none"
expect "$T/scrub-closed.out" tabs "journal.txt"
expect "$T/scrub-closed.out" cursor "4:1"
expect "$T/scrub-closed.out" dirty "false"
expect_line "$T/scrub-closed.out" 7 "zeta"
[ "$(cat "$T/scrubproj/journal.txt")" = "$(printf 'zero\nalpha\nBETA\ngamma\ndelta\nepsilon\nzeta')" ] \
    || failed "scrubber: journal.txt was written: $(cat "$T/scrubproj/journal.txt")"
[ "$(git -C "$T/scrubproj" status --porcelain | tr '\n' ' ')" = " M journal.txt ?? untracked.txt " ] \
    || failed "scrubber: the repository changed: $(git -C "$T/scrubproj" status --porcelain)"

# ---- the strip above the history ------------------------------------------------
# Older and Newer step as the arrows do; a click on the timeline goes to
# the version under it, the left end the oldest and the right end the
# working tree; a drag along it goes through the versions it crosses, the
# caret following its line; the close button is Escape. While the
# versions are listed the strip is over the file's own tab and says so.
cat > "$T/scrub-strip.script" <<SCRIPT
wait 800
wait 600
wait 300
key 125
key 125
key 125
key 16 cmd,opt y
dump $T/strip-listing.out
wait 800
dump $T/strip-open.out
click @scrub.older
wait 300
dump $T/strip-older.out
click @scrub.newer
wait 300
dump $T/strip-newer.out
clickat @scrub.track 0 0.5
wait 300
dump $T/strip-first.out
click @scrub.older
wait 100
dump $T/strip-first-older.out
clickat @scrub.track 1 0.5
wait 300
dump $T/strip-tree.out
downat @scrub.track 0 0.5
wait 300
dragat @scrub.track 0.25 0.5
wait 300
dump $T/strip-dragging.out
dragat @scrub.track 0.45 0.5
wait 300
upat @scrub.track 0.45 0.5
wait 300
dump $T/strip-dragged.out
click @scrub.close
wait 100
dump $T/strip-closed.out
quit
SCRIPT
CRC_SELFTEST="$T/scrub-strip.script" "$BIN" "$T/scrubproj/journal.txt" 2> "$T/scrub-strip.err"
# The list may be read before the first dump: only then is it checked.
if [ "$(grep -m1 '^scrub: ' "$T/strip-listing.out")" = "scrub: listing" ]; then
    expect "$T/strip-listing.out" scrub_strip "Reading the history of journal.txt | loading… | older=false newer=false"
    expect "$T/strip-listing.out" tabs "journal.txt"
fi
case "$(grep -m1 '^scrub: ' "$T/strip-open.out")" in
    "scrub: 5/5@$(scrub_hash HEAD) "*) ;;
    *) failed "scrubber strip: open: $(grep -m1 '^scrub: ' "$T/strip-open.out")" ;;
esac
case "$(grep -m1 '^scrub: ' "$T/strip-older.out")" in
    "scrub: 4/5@$(scrub_hash HEAD~1) "*) ;;
    *) failed "scrubber strip: Older: $(grep -m1 '^scrub: ' "$T/strip-older.out")" ;;
esac
expect "$T/strip-older.out" cursor "4:1"
case "$(grep -m1 '^scrub: ' "$T/strip-newer.out")" in
    "scrub: 5/5@$(scrub_hash HEAD) "*) ;;
    *) failed "scrubber strip: Newer: $(grep -m1 '^scrub: ' "$T/strip-newer.out")" ;;
esac
case "$(grep -m1 '^scrub: ' "$T/strip-first.out")" in
    "scrub: 1/5@$(scrub_hash HEAD~4) "*"first=alpha") ;;
    *) failed "scrubber strip: the left end: $(grep -m1 '^scrub: ' "$T/strip-first.out")" ;;
esac
case "$(grep -m1 '^scrub_strip: ' "$T/strip-first.out")" in
    "scrub_strip: $(scrub_hash HEAD~4)   Tester, "*" ago   one | 1 of 5 | older=false newer=true") ;;
    *) failed "scrubber strip: at the oldest: $(grep -m1 '^scrub_strip: ' "$T/strip-first.out")" ;;
esac
# Older at the oldest goes nowhere.
case "$(grep -m1 '^scrub: ' "$T/strip-first-older.out")" in
    "scrub: 1/5@$(scrub_hash HEAD~4) "*) ;;
    *) failed "scrubber strip: Older at the oldest: $(grep -m1 '^scrub: ' "$T/strip-first-older.out")" ;;
esac
expect "$T/strip-first-older.out" message "this is the oldest version"
case "$(grep -m1 '^scrub: ' "$T/strip-tree.out")" in
    "scrub: worktree/5 "*) ;;
    *) failed "scrubber strip: the right end: $(grep -m1 '^scrub: ' "$T/strip-tree.out")" ;;
esac
expect "$T/strip-tree.out" scrub_strip "journal.txt as open in its tab, after 5 commits | working tree | older=true newer=false"
expect_line "$T/strip-tree.out" 7 "zeta"
# Pressed at the left end, then dragged a quarter and nearly half way:
# the second and the third version, gamma's line followed from the first.
case "$(grep -m1 '^scrub: ' "$T/strip-dragging.out")" in
    "scrub: 2/5@$(scrub_hash HEAD~3) "*) ;;
    *) failed "scrubber strip: dragging: $(grep -m1 '^scrub: ' "$T/strip-dragging.out")" ;;
esac
case "$(grep -m1 '^scrub: ' "$T/strip-dragged.out")" in
    "scrub: 3/5@$(scrub_hash HEAD~2) "*) ;;
    *) failed "scrubber strip: dragged: $(grep -m1 '^scrub: ' "$T/strip-dragged.out")" ;;
esac
expect "$T/strip-dragged.out" cursor "3:1"
expect "$T/strip-dragged.out" tabs "journal.txt | journal.txt @ $(scrub_hash HEAD~2)"
expect "$T/strip-closed.out" scrub "closed"
expect "$T/strip-closed.out" scrub_strip "none"
expect "$T/strip-closed.out" tabs "journal.txt"
expect "$T/strip-closed.out" cursor "4:1"
expect "$T/strip-closed.out" dirty "false"

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
