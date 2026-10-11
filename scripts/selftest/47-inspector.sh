# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh inspector.

# ---- The workspace inspector: a branch's state and its Git buttons --------
# A repository with a bare origin, and a worktree on `feat` one commit
# ahead of main and one behind, with a new file not committed. Agents mode
# shows the inspector for the selected workspace. Commit All takes a
# message in the palette and commits the new file; Merge main In merges
# main into feat; Push sets feat's upstream on origin. A second launch,
# after someone else pushed to feat, pulls it.
P="$T/inspectproj"
O="$T/inspect-origin.git"
F="$T/inspect-wt/feat"
mkdir -p "$P" "$T/inspect-wt"
git init -q --bare -b main "$O"
printf 'one\n' > "$P/a.txt"
testgit "$P" init -q -b main
# The app commits with the repository's own settings: no global config here.
testgit "$P" config user.name Tester
testgit "$P" config user.email t@example.com
testgit "$P" config commit.gpgsign false
testgit "$P" add a.txt
testgit "$P" commit -qm one
testgit "$P" remote add origin "$O"
testgit "$P" push -q -u origin main
testgit "$P" worktree add -q -b feat "$F"
printf 'feature\n' > "$F/f.txt"
testgit "$F" add f.txt
testgit "$F" commit -qm feature
printf 'two\n' >> "$P/a.txt"
testgit "$P" commit -qam two
printf 'new\n' > "$F/new.txt"
cat > "$T/inspector.script" <<SCRIPT
wait 500
key 0 cmd,shift A
wait 700
wait 600
key 19 ctrl 2
wait 700
wait 500
dump $T/inspector-feat.out
click @inspector.commit
wait 200
text wip
key 36
wait 700
wait 700
dump $T/inspector-committed.out
click @inspector.update
wait 700
wait 700
dump $T/inspector-updated.out
click @inspector.push
wait 700
wait 700
wait 500
dump $T/inspector-pushed.out
key 34 cmd,opt i
wait 300
dump $T/inspector-hidden.out
quit
SCRIPT
CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/inspector.script" "$BIN" "$P" 2> "$T/inspector.err"
expect "$T/inspector-feat.out" agents "on tasks=main,feat selected=feat sessions=main,*feat claude=0 pair=- op=-"
expect_prefix() {
    local got
    got=$(grep -m1 "^$2: " "$1" 2>/dev/null | sed "s/^$2: //" || true)
    case "$got" in
    "$3"*) ;;
    *)
        echo "FAIL $(basename "$1"): $2 is [$got], expected it to start with [$3]"
        fail=1
        ;;
    esac
}
expect_prefix "$T/inspector-feat.out" inspector "branch=feat base=main ahead=1 behind=1 upstream=- push=0 pull=0 dirty=1 changes=+1-0 tag=- buttons=commit,update,push"
expect_prefix "$T/inspector-committed.out" inspector "branch=feat base=main ahead=2 behind=1 upstream=- push=0 pull=0 dirty=0 changes=+2-0 tag=- buttons=update,push"
subjects=$(testgit "$F" log --format=%s)
case "$subjects" in
*"
wip
"*) ;;
*) failed "inspector: Commit All did not commit: $subjects" ;;
esac
expect_prefix "$T/inspector-updated.out" inspector "branch=feat base=main ahead=3 behind=0 upstream=- push=0 pull=0 dirty=0"
expect_prefix "$T/inspector-pushed.out" inspector "branch=feat base=main ahead=3 behind=0 upstream=origin/feat push=0 pull=0 dirty=0"
git -C "$O" rev-parse -q --verify refs/heads/feat >/dev/null \
    || failed "inspector: Push did not put feat on origin"
# Cmd-Option-I hides it; the terminal takes the room back.
expect "$T/inspector-hidden.out" inspector "hidden"

# Someone else pushes to feat; the next launch has it to pull.
git clone -q -b feat "$O" "$T/inspect-other"
printf 'theirs\n' > "$T/inspect-other/t.txt"
testgit "$T/inspect-other" add t.txt
testgit "$T/inspect-other" commit -qm theirs
testgit "$T/inspect-other" push -q origin feat
testgit "$F" fetch -q origin
# A third worktree, after feat in Git's order; a session in it that ends
# its turn (the Stop hook) moves it above feat.
testgit "$P" worktree add -q -b zz "$T/inspect-wt/zz"
case "$BIN" in /*) inspect_hook=$BIN ;; *) inspect_hook=$PWD/$BIN ;; esac
mkdir -p "$T/inspectbin"
printf '#!/bin/sh\nprintf "{}" | "%s" --hook stop\nsleep 30\n' "$inspect_hook" > "$T/inspectbin/stopstub"
chmod +x "$T/inspectbin/stopstub"
cat > "$T/inspector-pull.script" <<SCRIPT
wait 500
key 0 cmd,shift A
wait 700
wait 600
key 19 ctrl 2
wait 700
wait 500
dump $T/inspector-behind.out
click @inspector.pull
wait 700
wait 700
dump $T/inspector-pulled.out
key 20 ctrl 3
wait 700
dump $T/inspector-zz.out
text stopstub
key 36
wait 700
wait 700
dump $T/inspector-sorted.out
quit
SCRIPT
PATH="$T/inspectbin:$PATH" CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/inspector-pull.script" \
    "$BIN" "$P" 2> "$T/inspector-pull.err"
expect_prefix "$T/inspector-behind.out" inspector "branch=feat base=main ahead=3 behind=0 upstream=origin/feat push=0 pull=1"
expect_prefix "$T/inspector-pulled.out" inspector "branch=feat base=main ahead=4 behind=0 upstream=origin/feat push=0 pull=0"
[ -f "$F/t.txt" ] || failed "inspector: Pull did not bring t.txt"
# Waiting on you sorts first, after the main checkout.
expect "$T/inspector-zz.out" agents "on tasks=main,feat,zz selected=zz sessions=main,feat,*zz claude=0 pair=- op=-"
expect "$T/inspector-sorted.out" agents "on tasks=main,zz,feat selected=zz sessions=main,feat,*zz claude=0 pair=- op=-"
testgit "$P" worktree remove --force "$F" 2>/dev/null || true
testgit "$P" worktree remove --force "$T/inspect-wt/zz" 2>/dev/null || true
