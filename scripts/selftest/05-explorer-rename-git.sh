# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh explorer-rename-git.

# ---- the name field says what is wrong with a name --------------------------
# The Explorer's field for Rename, New File and New Folder checks the name
# as it is typed and says what is wrong right under itself. Return does
# nothing while the name will not do; putting it right and Return takes it;
# Escape, or a click elsewhere, drops it. Every dump has the field's text
# (`sidebar_edit:`) and what it says (`tree_name_note:`, with `error: ` or
# `warning: ` in front, empty with nothing said), so an open field that
# refused Return is told from a closed one by its note.
#
# A right-click selects the one row it lands on without opening it, and
# Rename is then the menu item's action.
rg_project() {
    mkdir -p "$1/recipes"
    printf 'flour, water, salt\n' > "$1/recipes/bread.md"
    printf 'leeks and potatoes\n' > "$1/recipes/soup.md"
    printf 'tomatoes in may\n' > "$1/garden.txt"
    printf 'a quiet day\n' > "$1/journal.md"
}
# rg_names <dir>: the names in a folder exactly as the disk spells them,
# on one line.
rg_names() {
    ls -1 "$1" | tr '\n' ' ' | sed 's/ $//'
}
# rg_quiet <dump>: the status line does not report a failure.
rg_quiet() {
    if grep -m1 '^message: ' "$1" 2>/dev/null | grep -qiE 'fail|could not|already exists|not a valid'; then
        failed "$(basename "$1"): the status line says $(grep -m1 '^message: ' "$1")"
    fi
}

# A rename onto a name that is taken says so, and Return leaves the field
# open and the file alone. Fixed, Return renames. Then a change of case
# alone, which is the file itself and not in its own way.
rg_project "$T/renameproj"
cat > "$T/rename.script" <<SCRIPT
wait 500
click @sidebar.row.0
wait 500
menuat @sidebar.row.1 0.5 0.5
action renameProjectItem:
wait 200
dump $T/rename-open.out
text soup
wait 200
dump $T/rename-taken.out
key 36
wait 300
dump $T/rename-taken-return.out
key 36
wait 200
key 0 cmd a
text rye.md
wait 200
dump $T/rename-fixed.out
key 36
wait 700
dump $T/rename-done.out
menuat @sidebar.row.1 0.5 0.5
action renameProjectItem:
wait 200
key 0 cmd a
text Rye.md
wait 200
dump $T/rename-case.out
key 36
wait 700
dump $T/rename-case-done.out
quit
SCRIPT
CRC_SELFTEST="$T/rename.script" "$BIN" "$T/renameproj" 2> "$T/rename.err"
expect "$T/rename-open.out" sidebar_edit "bread.md"
expect "$T/rename-open.out" tree_name_note ""
# Outside a repository no row has a mark.
expect "$T/rename-open.out" tree_marks ""
expect "$T/rename-taken.out" sidebar_edit "soup.md"
expect "$T/rename-taken.out" tree_name_note "error: This name is already taken"
expect "$T/rename-taken-return.out" sidebar_edit "soup.md"
expect "$T/rename-taken-return.out" tree_name_note "error: This name is already taken"
expect "$T/rename-taken-return.out" file_ops ""
rg_quiet "$T/rename-taken-return.out"
expect "$T/rename-fixed.out" sidebar_edit "rye.md"
expect "$T/rename-fixed.out" tree_name_note ""
expect "$T/rename-done.out" sidebar_edit ""
expect "$T/rename-done.out" tree_rows "recipes/|*>recipes/rye.md|recipes/soup.md|garden.txt|journal.md"
expect "$T/rename-case.out" sidebar_edit "Rye.md"
expect "$T/rename-case.out" tree_name_note ""
expect "$T/rename-case-done.out" sidebar_edit ""
expect "$T/rename-case-done.out" tree_rows "recipes/|*>recipes/Rye.md|recipes/soup.md|garden.txt|journal.md"
rg_quiet "$T/rename-case-done.out"
if [ "$(rg_names "$T/renameproj/recipes")" != "Rye.md soup.md" ]; then
    failed "rename: recipes holds [$(rg_names "$T/renameproj/recipes")], expected [Rye.md soup.md]"
fi
if [ "$(cat "$T/renameproj/recipes/Rye.md" 2>/dev/null)" != "flour, water, salt" ]; then
    failed "rename: Rye.md is not the bread it was renamed from"
fi

# What a rename refuses, each said as it is typed: a dot name, a slash, no
# name at all. A colon is not one of them: the disk takes it, and nothing
# is said. Spaces at an end are a warning. Escape closes the field and
# nothing has moved. Then a name that is taken and a click on another row:
# the field is dropped without a word in the status line, and nothing is
# renamed.
rg_project "$T/refuseproj"
cat > "$T/refuse.script" <<SCRIPT
wait 500
click @sidebar.row.0
wait 500
menuat @sidebar.row.2 0.5 0.5
action renameProjectItem:
wait 200
key 0 cmd a
text ..
wait 200
dump $T/refuse-dots.out
key 0 cmd a
text stews/soup.md
wait 200
dump $T/refuse-slash.out
key 0 cmd a
text soup:leek.md
wait 200
dump $T/refuse-colon.out
key 0 cmd a
key 51
wait 200
dump $T/refuse-empty.out
key 36
wait 200
dump $T/refuse-empty-return.out
text  soup.md
wait 200
dump $T/refuse-space.out
key 53
wait 200
dump $T/refuse-escape.out
menuat @sidebar.row.2 0.5 0.5
action renameProjectItem:
wait 200
key 0 cmd a
text bread.md
wait 200
dump $T/refuse-click-typed.out
click @sidebar.row.4
wait 400
dump $T/refuse-click.out
quit
SCRIPT
CRC_SELFTEST="$T/refuse.script" "$BIN" "$T/refuseproj" 2> "$T/refuse.err"
expect "$T/refuse-dots.out" sidebar_edit ".."
expect "$T/refuse-dots.out" tree_name_note "error: . and .. are not names"
expect "$T/refuse-slash.out" tree_name_note "error: A name cannot contain /"
expect "$T/refuse-colon.out" sidebar_edit "soup:leek.md"
expect "$T/refuse-colon.out" tree_name_note ""
expect "$T/refuse-empty.out" tree_name_note "error: A name is required"
expect "$T/refuse-empty-return.out" tree_name_note "error: A name is required"
expect "$T/refuse-space.out" sidebar_edit " soup.md"
expect "$T/refuse-space.out" tree_name_note "warning: Starts or ends with a space"
expect "$T/refuse-escape.out" sidebar_edit ""
expect "$T/refuse-escape.out" tree_name_note ""
expect "$T/refuse-click-typed.out" tree_name_note "error: This name is already taken"
expect "$T/refuse-click.out" sidebar_edit ""
expect "$T/refuse-click.out" tree_name_note ""
expect "$T/refuse-click.out" file_ops ""
rg_quiet "$T/refuse-click.out"
if [ "$(rg_names "$T/refuseproj/recipes")" != "bread.md soup.md" ]; then
    failed "rename refused: recipes holds [$(rg_names "$T/refuseproj/recipes")], expected [bread.md soup.md]"
fi

# New File with no name: Return on the empty field does nothing but say
# that a name is required. A path that ends in a slash names nothing and
# is refused; a whole path is taken, with the folder on the way made. A
# new folder under a name that is taken says so. A name with a colon is
# made without a word, and one that starts with a space is warned about
# and made as typed.
rg_project "$T/newnameproj"
cat > "$T/newname.script" <<SCRIPT
wait 500
click 150 500
click @sidebar.action.0
wait 200
dump $T/newname-open.out
key 36
wait 200
dump $T/newname-empty.out
text plots/
wait 200
dump $T/newname-slash.out
key 36
wait 200
dump $T/newname-slash-return.out
key 0 cmd a
text plots/beds.md
wait 200
dump $T/newname-path.out
key 36
wait 800
dump $T/newname-made.out
click 150 500
click @sidebar.action.1
wait 200
text recipes
wait 200
dump $T/newname-folder.out
key 36
wait 200
dump $T/newname-folder-return.out
key 53
wait 200
click 150 500
click @sidebar.action.0
wait 200
text notes:today.md
wait 200
dump $T/newname-colon.out
key 36
wait 800
dump $T/newname-colon-made.out
click 150 500
click @sidebar.action.0
wait 200
text  lists.md
wait 200
dump $T/newname-space.out
key 36
wait 800
dump $T/newname-space-made.out
quit
SCRIPT
CRC_SELFTEST="$T/newname.script" "$BIN" "$T/newnameproj" 2> "$T/newname.err"
expect "$T/newname-open.out" sidebar_edit ""
expect "$T/newname-open.out" tree_name_note ""
expect "$T/newname-empty.out" sidebar_edit ""
expect "$T/newname-empty.out" tree_name_note "error: A name is required"
expect "$T/newname-slash.out" sidebar_edit "plots/"
expect "$T/newname-slash.out" tree_name_note "error: Every part of the path needs a name"
expect "$T/newname-slash-return.out" sidebar_edit "plots/"
expect "$T/newname-slash-return.out" tree_name_note "error: Every part of the path needs a name"
expect "$T/newname-path.out" sidebar_edit "plots/beds.md"
expect "$T/newname-path.out" tree_name_note ""
expect "$T/newname-made.out" sidebar_edit ""
expect "$T/newname-made.out" tabs "beds.md"
expect "$T/newname-folder.out" tree_name_note "error: This name is already taken"
expect "$T/newname-folder-return.out" sidebar_edit "recipes"
expect "$T/newname-folder-return.out" tree_name_note "error: This name is already taken"
rg_quiet "$T/newname-folder-return.out"
expect "$T/newname-colon.out" sidebar_edit "notes:today.md"
expect "$T/newname-colon.out" tree_name_note ""
expect "$T/newname-colon-made.out" sidebar_edit ""
rg_quiet "$T/newname-colon-made.out"
expect "$T/newname-space.out" sidebar_edit " lists.md"
expect "$T/newname-space.out" tree_name_note "warning: Starts or ends with a space"
expect "$T/newname-space-made.out" sidebar_edit ""
if [ ! -f "$T/newnameproj/plots/beds.md" ]; then
    failed "new file: plots/beds.md was not made"
fi
if [ ! -f "$T/newnameproj/notes:today.md" ]; then
    failed "new file: the name with a colon was not made: [$(rg_names "$T/newnameproj")]"
fi
if [ ! -f "$T/newnameproj/ lists.md" ]; then
    failed "new file: the name with a leading space was not made as typed: [$(rg_names "$T/newnameproj")]"
fi

# ---- Git's status on the rows -----------------------------------------------
# A changed file's row has a letter at its right edge and its name in the
# letter's colour; a folder with a change inside has a dot, in the colour
# of a conflict when it holds one; a file Git ignores is dimmed and has no
# mark. `tree_marks:` lists the rows that say something, by their path in
# the project: the letter, `.` for a folder's dot, `!` for the dot of a
# folder with a conflict inside, `i` for an ignored row. The marks come
# from the status Source Control already has: a file changed by another
# program gets its letter when that status is read again.
#
# The repository: one modified file, one untracked and one clean in
# recipes, a staged new file, a renamed one, an ignored one, and a merge
# that stopped on plots/tomatoes.md.
gm=$T/marksproj
mkdir -p "$gm/recipes" "$gm/plots" "$gm/trips"
printf 'flour, water, salt\n' > "$gm/recipes/bread.md"
printf 'leeks and potatoes\n' > "$gm/recipes/soup.md"
printf 'tomatoes: bed A\n' > "$gm/plots/tomatoes.md"
printf 'tram 28\n' > "$gm/trips/lisbon.md"
printf 'oat milk\n' > "$gm/shopping.txt"
printf 'a quiet day\n' > "$gm/journal.md"
printf 'dear anna\n' > "$gm/letters.txt"
printf 'scratch.log\n' > "$gm/.gitignore"
testgit "$gm" init -q -b main
testgit "$gm" add -A
testgit "$gm" commit -q -m "the notebook"
testgit "$gm" switch -q -c side
printf 'tomatoes: bed B\n' > "$gm/plots/tomatoes.md"
testgit "$gm" commit -q -am "bed B"
testgit "$gm" switch -q main
printf 'tomatoes: bed C\n' > "$gm/plots/tomatoes.md"
testgit "$gm" commit -q -am "bed C"
testgit "$gm" merge -q side > /dev/null 2>&1 && failed "marks: the fixture did not conflict"
printf 'and a little yeast\n' >> "$gm/recipes/bread.md"
printf 'eggs, milk, flour\n' > "$gm/recipes/pancakes.md"
printf 'tomatoes in may\n' > "$gm/garden.txt"
testgit "$gm" add garden.txt
testgit "$gm" mv shopping.txt groceries.txt
printf 'noise\n' > "$gm/scratch.log"
cat > "$T/marks.script" <<SCRIPT
wait 900
dump $T/marks-closed.out
click @sidebar.row.1
wait 600
click @sidebar.row.0
wait 600
dump $T/marks-open.out
wait 1200
wait 400
wait 400
wait 400
wait 400
dump $T/marks-later.out
quit
SCRIPT
# Another program edits a clean file once the rows have been dumped.
(
    for _ in $(seq 1 200); do
        [ -s "$T/marks-open.out" ] && break
        sleep 0.05
    done
    sleep 0.3
    printf 'the tram was full\n' >> "$gm/trips/lisbon.md"
) &
CRC_SELFTEST="$T/marks.script" "$BIN" "$gm" 2> "$T/marks.err"
wait
expect "$T/marks-closed.out" tree_rows "plots/|recipes/|trips/|.gitignore|garden.txt|groceries.txt|journal.md|letters.txt|scratch.log"
expect "$T/marks-closed.out" tree_marks "plots/=!|recipes/=.|garden.txt=A|groceries.txt=R|scratch.log=i"
expect "$T/marks-open.out" tree_marks "plots/=!|plots/tomatoes.md=C|recipes/=.|recipes/bread.md=M|recipes/pancakes.md=U|garden.txt=A|groceries.txt=R|scratch.log=i"
expect "$T/marks-later.out" tree_marks "plots/=!|plots/tomatoes.md=C|recipes/=.|recipes/bread.md=M|recipes/pancakes.md=U|trips/=.|garden.txt=A|groceries.txt=R|scratch.log=i"
# The same status Source Control counts.
expect "$T/marks-open.out" git_changes 5
expect "$T/marks-later.out" git_changes 6
unset gm
