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
click @toolbar.inspector
wait 300
dump $T/inspector-button-shown.out
click @toolbar.inspector
wait 300
dump $T/inspector-button-hidden.out
quit
SCRIPT
# The list is one for the app, kept under HOME: this scenario has its own.
mkdir -p "$T/home/inspector"
HOME="$T/home/inspector" CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/inspector.script" "$BIN" "$P" 2> "$T/inspector.err"
expect "$T/inspector-feat.out" agents "on tasks=inspectproj,feat selected=feat sessions=inspectproj,*feat claude=0 pair=- op=-"
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
# The toolbar's inspector button brings it back, and hides it again.
expect_prefix "$T/inspector-button-shown.out" inspector "branch=feat "
expect "$T/inspector-button-hidden.out" inspector "hidden"

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
HOME="$T/home/inspector" PATH="$T/inspectbin:$PATH" CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/inspector-pull.script" \
    "$BIN" "$P" 2> "$T/inspector-pull.err"
expect_prefix "$T/inspector-behind.out" inspector "branch=feat base=main ahead=3 behind=0 upstream=origin/feat push=0 pull=1"
expect_prefix "$T/inspector-pulled.out" inspector "branch=feat base=main ahead=4 behind=0 upstream=origin/feat push=0 pull=0"
[ -f "$F/t.txt" ] || failed "inspector: Pull did not bring t.txt"
# Waiting on you moves nothing: the list keeps your order.
expect "$T/inspector-zz.out" agents "on tasks=inspectproj,feat,zz selected=zz sessions=inspectproj,feat,*zz claude=0 pair=- op=-"
expect "$T/inspector-sorted.out" agents "on tasks=inspectproj,feat,zz selected=zz sessions=inspectproj,feat,*zz claude=0 pair=- op=-"
testgit "$P" worktree remove --force "$F" 2>/dev/null || true
testgit "$P" worktree remove --force "$T/inspect-wt/zz" 2>/dev/null || true

# ---- A folder of repositories: the inspector shows each one -------------
# A folder that is not a repository, holding two that are (and a plain
# folder and a hidden repository, which are not shown). Its workspace's
# inspector has a card per repository, with that one's buttons: Commit All
# on the dirty one commits there and nowhere else.
M="$T/inspect-many"
mkdir -p "$M/plain" "$M/.hidden"
for r in alpha beta .hidden; do
    mkdir -p "$M/$r"
    printf 'one\n' > "$M/$r/a.txt"
    testgit "$M/$r" init -q -b main
    testgit "$M/$r" config user.name Tester
    testgit "$M/$r" config user.email t@example.com
    testgit "$M/$r" config commit.gpgsign false
    testgit "$M/$r" add a.txt
    testgit "$M/$r" commit -qm one
done
printf 'two\n' >> "$M/beta/a.txt"
cat > "$T/inspector-many.script" <<SCRIPT
wait 500
key 0 cmd,shift A
wait 700
wait 700
wait 500
dump $T/inspector-many.out
click @inspector.commit.1
wait 200
text wip
key 36
wait 700
wait 700
dump $T/inspector-many-committed.out
quit
SCRIPT
mkdir -p "$T/home/inspector-many"
HOME="$T/home/inspector-many" CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/inspector-many.script" \
    "$BIN" "$M" 2> "$T/inspector-many.err"
expect "$T/inspector-many.out" agents "on tasks=inspect-many selected=inspect-many sessions=*inspect-many claude=0 pair=- op=-"
expect "$T/inspector-many.out" inspector "alpha: branch=main base=- ahead=0 behind=0 upstream=- push=0 pull=0 dirty=0 changes=+0-0 tag=- buttons=push | beta: branch=main base=- ahead=0 behind=0 upstream=- push=0 pull=0 dirty=1 changes=+0-0 tag=- buttons=commit,push"
[ "$(testgit "$M/beta" log -1 --format=%s)" = wip ] \
    || failed "inspector: Commit All in beta did not commit there: $(testgit "$M/beta" log -1 --format=%s)"
[ "$(testgit "$M/alpha" log -1 --format=%s)" = one ] \
    || failed "inspector: Commit All in beta committed in alpha"

# ---- A repository's card folds to its name row ----------------------------
# A click on alpha's name folds its card: no status read for it from then
# on, Refresh included, and the next launch remembers it. A click opens it
# again, and it is read.
cat > "$T/inspector-fold.script" <<SCRIPT
wait 500
key 0 cmd,shift A
wait 700
wait 700
click @inspector.repo_fold
wait 500
dump $T/inspector-folded.out
quit
SCRIPT
HOME="$T/home/inspector-many" CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/inspector-fold.script" \
    "$BIN" "$M" 2> "$T/inspector-fold.err"
inspector_beta="beta: branch=main base=- ahead=0 behind=0 upstream=- push=0 pull=0 dirty=0 changes=+0-0 tag=- buttons=push"
expect_prefix "$T/inspector-folded.out" inspector "alpha: folded reads="
case "$(grep -m1 '^inspector: ' "$T/inspector-folded.out")" in
*" | $inspector_beta") ;;
*) failed "inspector: beta is not read beside folded alpha: $(grep -m1 '^inspector: ' "$T/inspector-folded.out")" ;;
esac
cat > "$T/inspector-fold-kept.script" <<SCRIPT
wait 500
key 0 cmd,shift A
wait 700
click @inspector.refresh
wait 700
wait 700
dump $T/inspector-fold-kept.out
click @inspector.repo_fold
wait 700
wait 700
dump $T/inspector-unfolded.out
quit
SCRIPT
HOME="$T/home/inspector-many" CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/inspector-fold-kept.script" \
    "$BIN" "$M" 2> "$T/inspector-fold-kept.err"
expect "$T/inspector-fold-kept.out" inspector "alpha: folded reads=0 | $inspector_beta"
expect "$T/inspector-unfolded.out" inspector "alpha: branch=main base=- ahead=0 behind=0 upstream=- push=0 pull=0 dirty=0 changes=+0-0 tag=- buttons=push | $inspector_beta"
