# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh audit-crashes-and-data-loss.

# ---- the crashes and the data loss from the audit -------------------------
# Each of these used to end the process or throw work away. They are checked
# here, in the window, because that is where they happened.
mkdir -p "$T/proj"
printf 'caf\xc3\xa9 \xc3\xa9t\xc3\xa9\nplain\n' > "$T/proj/a.txt"
printf 'second file\n' > "$T/proj/b.txt"
cat > "$T/audit.script" <<SCRIPT
# Find with a query that starts with a multi-byte character (#1): the search
# used to resume inside the character and abort. Twice, to walk to the next.
key 3 cmd f
key 14 - \xc3\xa9
key 36
key 36
dump $T/find.out
key 53
# Type, then open another file from the palette (#12): it used to replace
# this buffer and everything typed into it, without a word.
click 344 280
text unsaved
key 35 cmd p
text b.txt
key 36
dump $T/opened.out
key 18 cmd 1
dump $T/kept.out
# Multiple cursors, an edit, undo, another edit (#4): the extra cursors used
# to be left pointing past the end of the restored text.
key 2 cmd d
key 2 cmd d
text zz
key 6 cmd z
text y
dump $T/cursors.out
quit
SCRIPT
# printf, so the \x escapes in the script become the bytes they name.
printf "$(cat "$T/audit.script")" > "$T/audit.script"
CRC_SELFTEST="$T/audit.script" "$BIN" "$T/proj/a.txt" 2> "$T/audit.err" || {
    echo "FAIL: the app died playing the audit script (exit $?)"
    fail=1
}
expect "$T/find.out" selection "\"$(printf '\xc3\xa9')\""
expect "$T/opened.out" tabs "a.txt | b.txt"
expect "$T/opened.out" active "1"
expect_line "$T/opened.out" 1 "second file"
expect "$T/kept.out" active "0"
expect "$T/kept.out" dirty "true"
if ! grep -q "unsaved" "$T/kept.out" 2>/dev/null; then
    failed "kept.out: the text typed before opening another file is gone"
fi
if [ ! -s "$T/cursors.out" ]; then
    failed "cursors.out: no dump, the app did not survive undo with extra cursors"
fi

# Markdown opens as styled text: the heading's marker and text have kinds
# of their own, and the fenced block a band. With no preview extension
# installed, Cmd-E says where to get one. Tabs can then be dragged to a new position without changing which
# file is active.
printf '# One\n\nBody\n\n```rust\nfn main() {}\n```\n' > "$T/proj/one.md"
printf 'two\n' > "$T/proj/two.txt"
cat > "$T/tabs.script" <<SCRIPT
key 11 cmd b
dump $T/source.out
key 14 cmd e
dump $T/preview.out
key 124 cmd
text X
dump $T/live-edit.out
key 35 cmd p
text two.txt
key 36
dump $T/two.out
down 94 62
drag 264 62
up 264 62
dump $T/reordered.out
click 744 62 2
dump $T/new-tab.out
quit
SCRIPT
CRC_SELFTEST="$T/tabs.script" "$BIN" "$T/proj/one.md" 2> "$T/tabs.err"
expect "$T/source.out" md "MdMarker,MdHeading bands=1"
expect "$T/preview.out" message "No preview installed: get Markdown Preview from crc > Extensions"
expect_line "$T/live-edit.out" 1 "# OneX"
expect "$T/two.out" tabs "one.md | two.txt"
expect "$T/reordered.out" tabs "two.txt | one.md"
expect "$T/reordered.out" active "1"
expect "$T/new-tab.out" tabs "two.txt | one.md | Untitled"
expect "$T/new-tab.out" active "2"

# Off the caret's line, Markdown's syntax takes no room: a click lands by
# what is drawn, "bold end", not by the stars that are hidden.
printf 'plain\n**bold** end\n' > "$T/conceal.md"
cat > "$T/conceal.script" <<SCRIPT
key 11 cmd b
click 79.2 144
dump $T/conceal-click.out
click 68 125
click 115.2 144 2
dump $T/conceal-word.out
quit
SCRIPT
CRC_SELFTEST="$T/conceal.script" "$BIN" "$T/conceal.md" 2> "$T/conceal.err"
expect "$T/conceal-click.out" cursor "2:4"
expect "$T/conceal-word.out" selection '"end"'

# AppKit re-enters the view whenever it likes: menus, modal panels, cursor
# rects, a redraw inside a call. With the state held as a caller up the
# stack holds it, a key, clicks, a context menu, menu actions, an input
# method query, a frame, close and quit must each skip, not abort, and
# leave the document as it was.
printf 'keep me\nsecond\n' > "$T/reenter.txt"
cat > "$T/reenter.script" <<SCRIPT
key 11 cmd b
reenter
dump $T/reenter.out
text y
dump $T/reenter-after.out
quit
SCRIPT
CRC_SELFTEST="$T/reenter.script" "$BIN" "$T/reenter.txt" 2> "$T/reenter.err"
expect "$T/reenter.out" message "reentered: drawn=false range=true close=false quit_cancelled=true"
expect "$T/reenter.out" dirty "false"
expect_line "$T/reenter.out" 1 "keep me"
expect_line "$T/reenter-after.out" 1 "ykeep me"

# Home is a page: typing there makes no Untitled document.
cat > "$T/home-typing.script" <<SCRIPT
wait 300
text abc
key 36
dump $T/home-typing.out
quit
SCRIPT
mkdir -p "$T/homeproj"
CRC_SELFTEST="$T/home-typing.script" "$BIN" "$T/homeproj" 2> "$T/home-typing.err"
expect "$T/home-typing.out" tabs "Home"
expect "$T/home-typing.out" dirty "false"

# Find fields must own the ordinary editing shortcuts, including Command-
# Backspace. Default find is case-insensitive and Return in Replace advances.
printf 'foo FOO\n' > "$T/find-fields.txt"
cat > "$T/find-fields.script" <<SCRIPT
key 3 cmd f
text garbage
key 0 cmd a
text foo
key 51 cmd
text foo
key 48
text X
key 36
dump $T/find-first.out
key 36
dump $T/find-second.out
quit
SCRIPT
CRC_SELFTEST="$T/find-fields.script" "$BIN" "$T/find-fields.txt" 2> "$T/find-fields.err"
expect_line "$T/find-first.out" 1 "X FOO"
expect_line "$T/find-second.out" 1 "X X"

# Project search opens the selected result in its file.
mkdir -p "$T/searchproj"
printf 'plain\n' > "$T/searchproj/one.txt"
printf 'unique needle here\n' > "$T/searchproj/two.txt"
cat > "$T/project-search.script" <<SCRIPT
key 3 cmd,shift f
text needle
key 36
wait 300
key 36
dump $T/project-result.out
quit
SCRIPT
CRC_SELFTEST="$T/project-search.script" "$BIN" "$T/searchproj/one.txt" 2> "$T/project-search.err"
expect "$T/project-result.out" tabs "one.txt | two.txt"
expect "$T/project-result.out" selection '"needle"'

# Replace All in project mode: the open file is changed in its tab and left
# unsaved, the file that is not open is written, the one without a match is
# untouched, and the list is searched again (and is empty).
mkdir -p "$T/replproj"
printf 'needle one\nneedle two\n' > "$T/replproj/a.txt"
printf 'a needle in b\n' > "$T/replproj/b.txt"
printf 'nothing here\n' > "$T/replproj/c.txt"
cat > "$T/project-replace.script" <<SCRIPT
wait 300
key 3 cmd,shift f
text needle
key 36
wait 300
wait 200
dump $T/project-replace-found.out
key 48
text thread
clickin @find 685 51
wait 300
wait 200
dump $T/project-replace.out
quit
SCRIPT
CRC_SELFTEST="$T/project-replace.script" "$BIN" "$T/replproj/a.txt" 2> "$T/project-replace.err"
expect "$T/project-replace-found.out" find_results "a.txt:1|a.txt:2|b.txt:1"
expect "$T/project-replace.out" message "replaced 3 in 2 files, 1 open and unsaved"
expect_line "$T/project-replace.out" 1 "thread one"
expect_line "$T/project-replace.out" 2 "thread two"
expect "$T/project-replace.out" dirty true
expect "$T/project-replace.out" find_results ""
[ "$(cat "$T/replproj/b.txt")" = "a thread in b" ] \
    || failed "project replace: b.txt is $(cat "$T/replproj/b.txt")"
[ "$(cat "$T/replproj/a.txt")" = "$(printf 'needle one\nneedle two')" ] \
    || failed "project replace: the open a.txt was written"
[ "$(cat "$T/replproj/c.txt")" = "nothing here" ] \
    || failed "project replace: c.txt changed"

# A directory argument opens that directory as the project, including its
# Cmd-P index, instead of restoring an unrelated session.
cat > "$T/folder-argument.script" <<SCRIPT
key 35 cmd p
text two.txt
key 36
dump $T/folder-argument.out
quit
SCRIPT
CRC_SELFTEST="$T/folder-argument.script" "$BIN" "$T/searchproj" 2> "$T/folder-argument.err"
expect "$T/folder-argument.out" tabs "two.txt"

# Expanding a folder uses a worker, then installs the child row in the sidebar.
mkdir -p "$T/treeproj/subdir"
printf 'EXAMPLE=value\n' > "$T/treeproj/subdir/.env"
printf 'visible\n' > "$T/treeproj/visible.txt"
cat > "$T/tree-expand.script" <<SCRIPT
wait 250
# The first tree row is the folder; once it opens, the next is its file.
click @sidebar.row.0
wait 250
click @sidebar.row.1
dump $T/tree-expand.out
key 35 cmd p
text visible.txt
key 36
key 35 cmd p
text .env
key 36
dump $T/tree-dotfile-finder.out
quit
SCRIPT
CRC_SELFTEST="$T/tree-expand.script" "$BIN" "$T/treeproj" 2> "$T/tree-expand.err"
expect "$T/tree-expand.out" tabs ".env"
expect_line "$T/tree-expand.out" 1 "EXAMPLE=value"
expect "$T/tree-dotfile-finder.out" tabs ".env | visible.txt"
expect "$T/tree-dotfile-finder.out" active "0"

# The toolbar project title must stay clickable in a narrow native window.
# Refresh should make an externally created file visible without scanning on
# the click's AppKit event thread.
mkdir -p "$T/personal-project-with-a-long-folder-name" "$T/titlehome"
printf 'personal sample\n' > "$T/personal-project-with-a-long-folder-name/seed.txt"
cat > "$T/title-menu.script" <<SCRIPT
wait 350
resize 464 450
dump $T/title-narrow.out
click 145 24
dump $T/title-menu.out
touch $T/personal-project-with-a-long-folder-name/after.txt
dump $T/title-stale.out
click @sidebar.action.3
wait 500
dump $T/title-refreshed.out
key 35 cmd p
text after.txt
key 36
dump $T/title-opened.out
click @breadcrumb.0
dump $T/title-crumb.out
quit
SCRIPT
HOME="$T/titlehome" CRC_SELFTEST="$T/title-menu.script" "$BIN" "$T/personal-project-with-a-long-folder-name" 2> "$T/title-menu.err"
if ! grep -q '^layout: window 464x450 sidebar Some(240' "$T/title-narrow.out"; then
    echo 'FAIL: narrow native window lost its sidebar or did not resize'
    fail=1
fi
expect "$T/title-menu.out" project_menu_requested true
expect "$T/title-stale.out" finder_entries 1
expect "$T/title-refreshed.out" finder_entries 2
expect "$T/title-opened.out" tabs after.txt
expect "$T/title-opened.out" key_handler_draws 0
# The breadcrumb's file part lists the folder it is in.
grep -q '^crumb_menu: .*/personal-project-with-a-long-folder-name$' "$T/title-crumb.out" \
    || failed "title-crumb: the breadcrumb did not offer its folder"
expect "$T/title-menu.out" crumb_menu none
expect "$T/title-opened.out" window_title after.txt

# A sidebar drag must never replace an existing entry, including a dangling
# symlink (which Path::exists reports as absent). Then check that a clear
# destination still accepts the same native mouse gesture.
mkdir -p "$T/moveproj/holder" "$T/movehome"
printf 'personal sample\n' > "$T/moveproj/source.txt"
ln -s missing-target "$T/moveproj/holder/source.txt"
cat > "$T/move.script" <<SCRIPT
wait 400
# Tree rows start below the 48pt toolbar and the 40pt sidebar header:
# row n is centred at 88 + 26n + 13. The file is row 1, the folder row 0.
down 104 127
drag 104 101
up 104 101
wait 400
quit
SCRIPT
HOME="$T/movehome" CRC_SELFTEST="$T/move.script" "$BIN" "$T/moveproj" 2> "$T/move-blocked.err"
if [ "$(readlink "$T/moveproj/holder/source.txt")" != 'missing-target' ] || [ ! -f "$T/moveproj/source.txt" ]; then
    echo 'FAIL: native sidebar drag replaced an existing symlink'
    fail=1
fi
rm "$T/moveproj/holder/source.txt"
HOME="$T/movehome" CRC_SELFTEST="$T/move.script" "$BIN" "$T/moveproj" 2> "$T/move-allowed.err"
if [ -e "$T/moveproj/source.txt" ] || [ "$(cat "$T/moveproj/holder/source.txt")" != 'personal sample' ]; then
    echo 'FAIL: native sidebar drag did not move into a clear folder'
    fail=1
fi

# Quick Look previews stay inside the app as read-only tabs.
printf '%s' 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a1ZkAAAAASUVORK5CYII=' | base64 -D > "$T/picture.png"
printf '<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24"><rect width="24" height="24" fill="red"/></svg>\n' > "$T/drawing.svg"
sips -s format pdf "$T/picture.png" --out "$T/document.pdf" >/dev/null
for kind in png svg pdf; do
    case "$kind" in
        png) file=picture.png ;;
        svg) file=drawing.svg ;;
        pdf) file=document.pdf ;;
    esac
    cat > "$T/preview.script" <<SCRIPT
dump $T/preview-$kind.out
quit
SCRIPT
    CRC_SELFTEST="$T/preview.script" "$BIN" "$T/$file" 2> "$T/preview-$kind.err"
    # SVG is XML, opened as text to edit; the others are pictures.
    if [ "$kind" = svg ]; then
        expect "$T/preview-$kind.out" native_preview false
    else
        expect "$T/preview-$kind.out" native_preview true
    fi
    expect "$T/preview-$kind.out" dirty false
done

# Native toolbar, modal mouse hit testing and local Git actions.
mkdir -p "$T/gitproj" "$T/githome"
git -C "$T/gitproj" init -q
git -C "$T/gitproj" config user.name 'caio GUI test'
git -C "$T/gitproj" config user.email 'test@example.invalid'
git -C "$T/gitproj" config commit.gpgsign false
git -C "$T/gitproj" config core.hooksPath .git/hooks
printf 'personal sample\n' > "$T/gitproj/sample.txt"
cat > "$T/git-ui.script" <<SCRIPT
wait 400
# Open through the native toolbar, then click the result.
click 950 24
text sample.txt
click 400 160
dump $T/palette-click.out
key 5 cmd,opt g
wait 500
dump $T/git-status.out
# Source control is docked in the 240pt sidebar. One untracked file means
# a section heading in row 0 and the file in row 1, whose staging control
# is at its trailing edge. Rows and controls are clicked by name.
click @git.toggle.1
wait 500
dump $T/git-stage.out
click @git.toggle.1
wait 500
dump $T/git-unstage.out
click @git.toggle.1
wait 500
# Git answers on its own thread; under load it can take longer than the
# wait, and the row would still read as unstaged when it is clicked.
idle 1000
# Click the row body, not the control: it selects and opens the diff.
click @git.row.1
wait 500
dump $T/git-diff.out
click @git.message
text Personal commit
dump $T/git-message.out
click @git.commit
wait 700
dump $T/git-commit.out
# Escape steps back out: first the message field, then the diff, then the
# view itself.
key 53
key 53
key 53
dump $T/git-closed.out
quit
SCRIPT
# A private HOME, so the restored session is the default 240pt sidebar rather
# than whatever width this machine's user last dragged it to. The coordinates
# below are relative to that width; the repository's own identity is set on
# the test repo above, not in a global config.
HOME="$T/githome" CRC_SELFTEST="$T/git-ui.script" "$BIN" "$T/gitproj" 2> "$T/git-ui.err"
expect "$T/palette-click.out" tabs sample.txt
expect "$T/git-status.out" git_open true
expect "$T/git-status.out" git_changes 1
expect "$T/git-stage.out" git_staged 1
expect "$T/git-unstage.out" git_staged 0
# The row body opens the diff; the editor column is no longer blocked by a
# modal, so the document stays open behind it.
expect "$T/git-diff.out" git_diff true
# The diff has a tab of its own, beside the document.
expect "$T/git-diff.out" tabs "sample.txt | sample.txt (Staged)"
expect "$T/git-message.out" git_focus true
expect "$T/git-message.out" git_message 'Personal commit'
expect "$T/git-message.out" dirty false
expect "$T/git-commit.out" git_changes 0
expect "$T/git-commit.out" git_message ''
expect "$T/git-closed.out" git_open false
expect "$T/git-closed.out" tabs "sample.txt"
expect "$T/git-closed.out" git_diff false
if [ "$(git -C "$T/gitproj" log -1 --format=%s)" != 'Personal commit' ]; then
    echo 'FAIL: native commit action did not create the expected commit'
    fail=1
fi

# Stage one of two distant edits from the diff column, then unstage it. The
# buttons are on the hunk header drawn at y=136 in the 1100x760 native window.
mkdir -p "$T/hunkproj" "$T/hunkhome"
git -C "$T/hunkproj" init -q
git -C "$T/hunkproj" config user.name 'caio GUI test'
git -C "$T/hunkproj" config user.email 'test@example.invalid'
git -C "$T/hunkproj" config commit.gpgsign false
for n in $(seq 1 24); do printf 'line %s\n' "$n"; done > "$T/hunkproj/personal notes.txt"
git -C "$T/hunkproj" add -- 'personal notes.txt'
git -C "$T/hunkproj" commit -q -m 'Personal hunk sample'
awk 'NR == 2 {$0 = "first edit"} NR == 21 {$0 = "second edit"} {print}' \
    "$T/hunkproj/personal notes.txt" > "$T/hunk-edited.txt"
mv "$T/hunk-edited.txt" "$T/hunkproj/personal notes.txt"
cat > "$T/hunk-ui.script" <<SCRIPT
wait 350
key 5 cmd,opt g
wait 500
click @git.row.1
wait 500
dump $T/hunk-before.out
click 1050 145
wait 500
dump $T/hunk-staged.out
click 1050 145
wait 500
dump $T/hunk-unstaged.out
quit
SCRIPT
HOME="$T/hunkhome" CRC_SELFTEST="$T/hunk-ui.script" "$BIN" "$T/hunkproj" 2> "$T/hunk-ui.err"
expect "$T/hunk-before.out" git_hunks_staged 0
expect "$T/hunk-before.out" git_hunks_working 2
expect "$T/hunk-staged.out" git_staged 1
expect "$T/hunk-staged.out" git_hunks_staged 1
expect "$T/hunk-staged.out" git_hunks_working 1
expect "$T/hunk-unstaged.out" git_staged 0
expect "$T/hunk-unstaged.out" git_hunks_working 2
if [ -n "$(git -C "$T/hunkproj" diff --cached)" ]; then
    echo 'FAIL: native hunk unstage left changes in the index'
    fail=1
fi
if ! git -C "$T/hunkproj" diff -- 'personal notes.txt' | grep -q '+second edit'; then
    echo 'FAIL: hunk actions changed the working tree'
    fail=1
fi
