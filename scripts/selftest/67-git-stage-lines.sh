# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh stage-lines.

# ---- staging lines from the diff with Space ----------------------------------
# A click on a diff row puts the cursor there and Space stages that line;
# shift-click selects lines and Space stages only the changed ones among
# them; in the staged view Space unstages. Space still types in the commit
# message and in a document with Source Control open.
mkdir -p "$T/lineproj" "$T/linehome"
git -C "$T/lineproj" init -q
git -C "$T/lineproj" config user.name 'caio GUI test'
git -C "$T/lineproj" config user.email 'test@example.invalid'
git -C "$T/lineproj" config commit.gpgsign false
printf 'beans\npeas\nmint\n' > "$T/lineproj/garden.txt"
git -C "$T/lineproj" add garden.txt
git -C "$T/lineproj" commit -q -m 'start the garden'
printf 'beans\ncarrots\npeas\nleeks\nchard\n' > "$T/lineproj/garden.txt"
# Diff rows, from 0: the section, the hunk, beans, +carrots, peas, -mint,
# +leeks, +chard.
cat > "$T/lines.script" <<SCRIPT
wait 400
key 5 cmd,opt g
wait 600
click @git.message
text fix the beds
wait 100
dump $T/lines-message.out
click @git.row.1
wait 600
idle 1000
click @git.diff.3
wait 100
dump $T/lines-cursor.out
key 49
wait 600
idle 1000
dump $T/lines-one.out
click @git.diff.10
clickin @git.diff.11 10 10 1 shift
wait 100
dump $T/lines-range.out
key 49
wait 600
idle 1000
dump $T/lines-range-staged.out
click @git.diff.2
key 125
wait 100
dump $T/lines-down.out
key 49
wait 600
idle 1000
dump $T/lines-unstaged.out
click @git.diff.10
clickin @git.diff.11 10 10 1 shift
wait 100
idle 500
frame $T/lines-selection.bmp
quit
SCRIPT
HOME="$T/linehome" CRC_SELFTEST="$T/lines.script" "$BIN" "$T/lineproj" 2> "$T/lines.err"
expect "$T/lines-message.out" git_message 'fix the beds'
expect "$T/lines-cursor.out" git_diff true
expect "$T/lines-cursor.out" git_focus false
expect "$T/lines-cursor.out" git_diff_selection '3-3 cursor 3'
if [ "$(git -C "$T/lineproj" show :garden.txt | tr '\n' ' ')" != 'beans peas ' ]; then
    echo "FAIL: line staging left the index at [$(git -C "$T/lineproj" show :garden.txt | tr '\n' ' ')], expected [beans peas ]"
    fail=1
fi
expect "$T/lines-one.out" git_staged 1
expect "$T/lines-one.out" git_hunks_staged 1
expect "$T/lines-range.out" git_diff_selection '10-11 cursor 11'
expect "$T/lines-range-staged.out" git_hunks_staged 1
# The cursor stays where the selection started, the diff read again.
expect "$T/lines-range-staged.out" git_diff_selection '10-10 cursor 10'
expect "$T/lines-down.out" git_diff_selection '3-3 cursor 3'
expect "$T/lines-unstaged.out" git_hunks_staged 1
expect "$T/lines-unstaged.out" git_diff_selection '3-3 cursor 3'
if ! git -C "$T/lineproj" diff --cached | grep -q '^-mint'; then
    echo 'FAIL: the shift-selected removal is not staged'
    fail=1
fi
if git -C "$T/lineproj" diff --cached | grep -q '^+[^+]'; then
    echo 'FAIL: Space in the staged view did not unstage the added line'
    fail=1
fi
if [ "$(tr '\n' ' ' < "$T/lineproj/garden.txt")" != 'beans carrots peas leeks chard ' ]; then
    echo 'FAIL: line staging changed the working tree'
    fail=1
fi

# Space with Source Control open and a document in the column types a space.
printf 'one\n' > "$T/lineproj/notes.txt"
cat > "$T/lines-doc.script" <<SCRIPT
wait 400
key 5 cmd,opt g
wait 600
text a b
wait 100
dump $T/lines-doc.out
quit
SCRIPT
HOME="$T/linehome" CRC_SELFTEST="$T/lines-doc.script" "$BIN" "$T/lineproj/notes.txt" 2> "$T/lines-doc.err"
expect "$T/lines-doc.out" git_open true
expect "$T/lines-doc.out" git_diff false
expect_line "$T/lines-doc.out" 1 'a bone'
