# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh git-amend.

# ---- amending the last commit ------------------------------------------------
# Amend sits beside Commit. With HEAD already on its upstream it is off
# and says why. Once a local commit is ahead of the upstream it can be
# turned on: the empty message field starts from HEAD's message, the
# button reads Amend, and Cmd-Return replaces HEAD with the staged change
# in it, without adding a commit.
mkdir -p "$T/amendhome"
git init -q --bare "$T/amend-origin.git"
git clone -q "$T/amend-origin.git" "$T/amendproj" 2> /dev/null
git -C "$T/amendproj" config user.name 'caio GUI test'
git -C "$T/amendproj" config user.email 'test@example.invalid'
git -C "$T/amendproj" config commit.gpgsign false
git -C "$T/amendproj" config core.hooksPath .git/hooks
printf 'beans\n' > "$T/amendproj/garden.txt"
git -C "$T/amendproj" add garden.txt
git -C "$T/amendproj" commit -q -m 'start the garden'
git -C "$T/amendproj" push -q -u origin HEAD 2> /dev/null
cat > "$T/amend-pushed.script" <<SCRIPT
wait 400
key 5 cmd,opt g
wait 600
idle 1000
dump $T/amend-pushed.out
frame $T/amend-blocked.bmp
click @git.amend
wait 100
dump $T/amend-pushed-click.out
quit
SCRIPT
HOME="$T/amendhome" CRC_SELFTEST="$T/amend-pushed.script" "$BIN" "$T/amendproj" 2> "$T/amend-pushed.err"
expect "$T/amend-pushed.out" git_open true
expect "$T/amend-pushed.out" git_amend blocked
# On its upstream with nothing to move: the arrows stay bare.
expect "$T/amend-pushed.out" git_sync "↓ ↑ not checked"
expect "$T/amend-pushed-click.out" git_amend blocked
expect "$T/amend-pushed-click.out" git_message ''

# A local commit ahead of the upstream, and a change staged to fold into it.
printf 'water\n' > "$T/amendproj/water.txt"
git -C "$T/amendproj" add water.txt
git -C "$T/amendproj" commit -q -m 'plant the beans'
printf 'sun\n' > "$T/amendproj/sun.txt"
git -C "$T/amendproj" add sun.txt
cat > "$T/amend-local.script" <<SCRIPT
wait 400
key 5 cmd,opt g
wait 600
idle 1000
dump $T/amend-off.out
frame $T/amend-off.bmp
click @git.amend
wait 100
dump $T/amend-on.out
click @git.message
key 119
text  and water them
wait 100
dump $T/amend-edited.out
frame $T/amend-on.bmp
key 36 cmd
wait 700
idle 1000
dump $T/amend-done.out
quit
SCRIPT
HOME="$T/amendhome" CRC_SELFTEST="$T/amend-local.script" "$BIN" "$T/amendproj" 2> "$T/amend-local.err"
expect "$T/amend-off.out" git_amend off
expect "$T/amend-off.out" git_staged 1
# One commit ahead: Push carries the count.
expect "$T/amend-off.out" git_sync "↓ ↑1 not checked"
expect "$T/amend-on.out" git_amend on
expect "$T/amend-on.out" git_message 'plant the beans'
expect "$T/amend-edited.out" git_focus true
expect "$T/amend-edited.out" git_message 'plant the beans and water them'
expect "$T/amend-done.out" git_amend off
expect "$T/amend-done.out" git_message ''
expect "$T/amend-done.out" git_changes 0
subject=$(git -C "$T/amendproj" log -1 --format=%s)
[ "$subject" = 'plant the beans and water them' ] || failed "amend: HEAD's subject is [$subject]"
commits=$(git -C "$T/amendproj" rev-list --count HEAD)
[ "$commits" = 2 ] || failed "amend: $commits commits, expected 2 (amended, not added)"
files=$(git -C "$T/amendproj" show --name-only --format= HEAD | tr '\n' ' ')
[ "$files" = 'sun.txt water.txt ' ] || failed "amend: HEAD holds [$files], expected the staged sun.txt folded in"
[ -z "${SELFTEST_FRAMES:-}" ] || cp -f "$T/amend-blocked.bmp" "$T/amend-off.bmp" "$T/amend-on.bmp" "$SELFTEST_FRAMES/"
