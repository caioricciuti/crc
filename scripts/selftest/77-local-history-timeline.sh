# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh local-history.

# ---- local versions in File History ---------------------------------------------
# plan.txt is committed once, then edited and saved twice in crc, then
# committed again from the shell. File History shows one timeline: the
# first commit, the two saves, the second commit; the text it was opened
# with is the first commit's and adds nothing. A local version has its
# own pill and says why it was kept; the filter steps through commits or
# local versions alone, keeps the version shown when it can, and the
# next history opened in the window starts with it. A file outside Git
# has its local versions alone; one with none says it is not in Git.
mkdir -p "$T/lhproj" "$T/lhloose" "$T/lh"
lhdb="$T/lh/history.db"
git init -q -b main "$T/lhproj"
git -C "$T/lhproj" config core.autocrlf false
printf 'alpha\n' > "$T/lhproj/plan.txt"
git -C "$T/lhproj" add plan.txt
GIT_AUTHOR_DATE=2024-03-01T12:00:00Z GIT_COMMITTER_DATE=2024-03-01T12:00:00Z \
    testgit "$T/lhproj" commit -q -m first
printf 'loose\n' > "$T/lhloose/memo.txt"
printf 'bare\n' > "$T/lhloose/bare.txt"
lh_hash() { git -C "$T/lhproj" rev-parse --short=7 "$1"; }
# lh_clock <file> <field> <before> <after>: the field is <before>, a
# 24-hour clock time and <after>, as a local version made today says when.
lh_clock() {
    local got
    got=$(grep -m1 "^$2: " "$1" 2>/dev/null | sed "s/^$2: //" || true)
    case "$got" in
        "$3"[0-2][0-9]:[0-5][0-9]"$4") ;;
        *) failed "$(basename "$1"): $2 is [$got], expected [$3HH:MM$4]" ;;
    esac
}

# Two saves of plan.txt, and one of memo.txt, into the run's history.
cat > "$T/lh-edit.script" <<SCRIPT
wait 800
text x
key 1 cmd s
wait 300
text y
key 1 cmd s
wait 300
quit
SCRIPT
CRC_HISTORY_DB="$lhdb" CRC_SELFTEST="$T/lh-edit.script" "$BIN" "$T/lhproj/plan.txt" 2> "$T/lh-edit.err"
cat > "$T/lh-edit-loose.script" <<SCRIPT
wait 800
text z
key 1 cmd s
wait 300
quit
SCRIPT
CRC_HISTORY_DB="$lhdb" CRC_SELFTEST="$T/lh-edit-loose.script" "$BIN" "$T/lhloose/memo.txt" 2> "$T/lh-edit-loose.err"
[ "$(cat "$T/lhproj/plan.txt")" = "xyalpha" ] \
    || failed "lh: plan.txt was not saved: $(cat "$T/lhproj/plan.txt")"
# The second commit, from the shell, after the saves.
printf 'omega\n' > "$T/lhproj/plan.txt"
testgit "$T/lhproj" commit -q -am second

cat > "$T/lh.script" <<SCRIPT
wait 800
wait 300
key 16 cmd,opt y
wait 800
wait 300
dump $T/lh-open.out
key 123
wait 300
dump $T/lh-local.out
key 123
wait 300
dump $T/lh-local-older.out
click @scrub.filter
wait 300
wait 300
dump $T/lh-commits.out
click @scrub.filter
wait 300
wait 300
dump $T/lh-only-local.out
key 123
wait 300
dump $T/lh-only-local-older.out
click @scrub.filter
wait 300
wait 300
dump $T/lh-all.out
click @scrub.filter
wait 300
key 53
wait 100
dump $T/lh-closed.out
key 16 cmd,opt y
wait 800
wait 300
dump $T/lh-reopened.out
quit
SCRIPT
CRC_HISTORY_DB="$lhdb" CRC_SELFTEST="$T/lh.script" "$BIN" "$T/lhproj/plan.txt" 2> "$T/lh.err"
c1=$(lh_hash HEAD~1)
c2=$(lh_hash HEAD)
case "$(grep -m1 '^scrub: ' "$T/lh-open.out")" in
    "scrub: 4/4@$c2 "*) ;;
    *) failed "lh: open: $(grep -m1 '^scrub: ' "$T/lh-open.out")" ;;
esac
expect "$T/lh-open.out" scrub_filter "all 4/4 $c1,local:saved,local:saved,$c2"
# One step older: the second save, its own pill and words, and what it
# changed against the save before it.
case "$(grep -m1 '^scrub: ' "$T/lh-local.out")" in
    "scrub: 3/4@local:saved "*"first=xyalpha") ;;
    *) failed "lh: the second save: $(grep -m1 '^scrub: ' "$T/lh-local.out")" ;;
esac
lh_clock "$T/lh-local.out" scrub_strip "saved, " " | 3 of 4 | older=true newer=true"
lh_clock "$T/lh-local.out" message "local version, saved, " " (3 of 4)"
lh_clock "$T/lh-local.out" tabs "plan.txt | plan.txt @ saved " ""
expect "$T/lh-local.out" gutter "1M inactive"
case "$(grep -m1 '^scrub: ' "$T/lh-local-older.out")" in
    "scrub: 2/4@local:saved "*"first=xalpha") ;;
    *) failed "lh: the first save: $(grep -m1 '^scrub: ' "$T/lh-local-older.out")" ;;
esac
# Commits alone: the save shown is left out, and the next commit after it
# takes its place.
expect "$T/lh-commits.out" scrub_filter "commits 2/4 $c1,$c2"
case "$(grep -m1 '^scrub: ' "$T/lh-commits.out")" in
    "scrub: 2/2@$c2 "*"first=omega") ;;
    *) failed "lh: commits alone: $(grep -m1 '^scrub: ' "$T/lh-commits.out")" ;;
esac
expect_line "$T/lh-commits.out" 1 "omega"
# Local alone: nothing local after the second commit, so the working tree.
expect "$T/lh-only-local.out" scrub_filter "local 2/4 local:saved,local:saved"
case "$(grep -m1 '^scrub: ' "$T/lh-only-local.out")" in
    "scrub: worktree/2 "*) ;;
    *) failed "lh: local alone: $(grep -m1 '^scrub: ' "$T/lh-only-local.out")" ;;
esac
expect "$T/lh-only-local.out" scrub_strip "plan.txt as open in its tab, after 2 local versions | working tree | older=true newer=false"
case "$(grep -m1 '^scrub: ' "$T/lh-only-local-older.out")" in
    "scrub: 2/2@local:saved "*"first=xyalpha") ;;
    *) failed "lh: local alone, one older: $(grep -m1 '^scrub: ' "$T/lh-only-local-older.out")" ;;
esac
# All again: the save shown keeps its place.
expect "$T/lh-all.out" scrub_filter "all 4/4 $c1,local:saved,local:saved,$c2"
case "$(grep -m1 '^scrub: ' "$T/lh-all.out")" in
    "scrub: 3/4@local:saved "*"first=xyalpha") ;;
    *) failed "lh: all again: $(grep -m1 '^scrub: ' "$T/lh-all.out")" ;;
esac
expect "$T/lh-closed.out" scrub "closed"
# Left at Commits, and opened again with it.
expect "$T/lh-reopened.out" scrub_filter "commits 2/4 $c1,$c2"
case "$(grep -m1 '^scrub: ' "$T/lh-reopened.out")" in
    "scrub: 2/2@$c2 "*) ;;
    *) failed "lh: reopened: $(grep -m1 '^scrub: ' "$T/lh-reopened.out")" ;;
esac
[ "$(cat "$T/lhproj/plan.txt")" = "omega" ] \
    || failed "lh: plan.txt was written: $(cat "$T/lhproj/plan.txt")"

# Outside Git: memo.txt's local versions alone, the filter with nothing
# to choose; bare.txt has none, and is not in Git.
cat > "$T/lh-loose.script" <<SCRIPT
wait 800
wait 300
key 16 cmd,opt y
wait 800
wait 300
dump $T/lh-loose.out
key 123
wait 300
dump $T/lh-loose-older.out
click @scrub.filter
wait 100
dump $T/lh-loose-filter.out
quit
SCRIPT
CRC_HISTORY_DB="$lhdb" CRC_SELFTEST="$T/lh-loose.script" "$BIN" "$T/lhloose/memo.txt" 2> "$T/lh-loose.err"
expect "$T/lh-loose.out" scrub_filter "all 2/2 local:opened,local:saved"
case "$(grep -m1 '^scrub: ' "$T/lh-loose.out")" in
    "scrub: 2/2@local:saved "*"first=zloose") ;;
    *) failed "lh: outside Git: $(grep -m1 '^scrub: ' "$T/lh-loose.out")" ;;
esac
lh_clock "$T/lh-loose.out" tabs "memo.txt | memo.txt @ saved " ""
case "$(grep -m1 '^scrub: ' "$T/lh-loose-older.out")" in
    "scrub: 1/2@local:opened "*"first=loose") ;;
    *) failed "lh: outside Git, opened: $(grep -m1 '^scrub: ' "$T/lh-loose-older.out")" ;;
esac
lh_clock "$T/lh-loose-older.out" scrub_strip "opened, " " | 1 of 2 | older=false newer=true"
expect "$T/lh-loose-filter.out" message "memo.txt has no commits to filter by"
expect "$T/lh-loose-filter.out" scrub_filter "all 2/2 local:opened,local:saved"
cat > "$T/lh-bare.script" <<SCRIPT
wait 800
wait 300
key 16 cmd,opt y
wait 800
dump $T/lh-bare.out
quit
SCRIPT
CRC_HISTORY_DB="$lhdb" CRC_SELFTEST="$T/lh-bare.script" "$BIN" "$T/lhloose/bare.txt" 2> "$T/lh-bare.err"
expect "$T/lh-bare.out" message "this file is not in a Git repository"
expect "$T/lh-bare.out" scrub "closed"
expect "$T/lh-bare.out" tabs "bare.txt"
