#!/usr/bin/env bash
# Plays input into the real app and checks what came out.
#
# The unit tests cannot reach window.rs, which is behind AppKit, and the bugs
# that mattered have been there: clicks landing rows away from the pointer
# under a fully green suite. This drives the actual window with real NSEvents
# (see src/platform/selftest.rs) and reads the document back.
#
# It opens a window for a few seconds. It does not need the app to be in
# front, and it never touches the saved session: the app is launched on a
# file, which makes the session ephemeral, and quits without saving.
#
# What it cannot check is anything that needs the app to be frontmost, which
# includes dead keys: the input system only composes for the active app.
#
# Usage: scripts/selftest.sh        (builds first)
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release --locked
BIN=target/release/crc
T=$(mktemp -d)
trap 'result=$?; if [ "$result" -eq 0 ]; then rm -rf "$T"; else echo "Self-test failure artifacts: $T"; fi' EXIT
fail=0
# Every launch serves Claude Code for its project. Keep the lock files out of
# the real ~/.claude/ide, where a running claude would offer these windows.
export CLAUDE_CONFIG_DIR="$T/claude-config"
# The person running this has settings of their own: a zoomed font moves
# every click target by a column, a light theme changes what a dump says.
# Every launch gets an empty home, so the checks below see the defaults.
# Scenarios that need a settings file make one under their own HOME.
mkdir -p "$T/home"
export HOME="$T/home"
unset XDG_CONFIG_HOME

# expect <file> <field> <value>
expect() {
    local got
    got=$(grep -m1 "^$2: " "$1" | sed "s/^$2: //")
    if [ "$got" != "$3" ]; then
        echo "FAIL $(basename "$1"): $2 is [$got], expected [$3]"
        fail=1
    fi
}
# expect_line <file> <n> <text>: line n of the document
expect_line() {
    local got
    got=$(sed -n '/^text:$/,$p' "$1" | sed -n "$(($2 + 1))p")
    if [ "$got" != "$3" ]; then
        echo "FAIL $(basename "$1"): line $2 is [$got], expected [$3]"
        fail=1
    fi
}

# Geometry the scripts rely on, with the sidebar hidden: a one-digit gutter is
# 3 cells of 8pt, the text starts 116pt down (toolbar 48 + tabs 30 + breadcrumbs 30 + 8 of air), and a
# line is 19pt. So line n is centred at y = 116 + 19 * (n - 1) + 9.

# ---- typing ---------------------------------------------------------------
: > "$T/typing.txt"
cat > "$T/typing.script" <<SCRIPT
text let x = 1;
key 36
text ok
dump $T/typing.out
quit
SCRIPT
CRC_SELFTEST="$T/typing.script" "$BIN" "$T/typing.txt" 2> "$T/typing.err"
expect_line "$T/typing.out" 1 "let x = 1;"
expect_line "$T/typing.out" 2 "ok"
expect "$T/typing.out" cursor "2:3"
expect "$T/typing.out" dirty "true"

# ---- files changed behind the editor --------------------------------------
# A clean tab follows the disk; a tab with unsaved changes keeps them and is
# told. This is the path Claude Code and git checkout take.
printf 'first\n' > "$T/behind.txt"
cat > "$T/behind.script" <<SCRIPT
write $T/behind.txt second
wait 100
dump $T/behind-clean.out
text local
write $T/behind.txt third
wait 100
dump $T/behind-dirty.out
key 6 cmd z
key 6 cmd z
dump $T/behind-undone.out
quit
SCRIPT
CRC_SELFTEST="$T/behind.script" "$BIN" "$T/behind.txt" 2> "$T/behind.err"
expect_line "$T/behind-clean.out" 1 "second"
expect "$T/behind-clean.out" dirty "false"
expect_line "$T/behind-dirty.out" 1 "localsecond"
expect "$T/behind-dirty.out" dirty "true"
if [ "$(sed -n 1p "$T/behind.txt")" != "third" ]; then
    echo "FAIL behind: the external write was replaced by [$(sed -n 1p "$T/behind.txt")]"
    fail=1
fi
# Two undos: the typed word, then the reload itself.
expect_line "$T/behind-undone.out" 1 "first"
expect "$T/behind-undone.out" dirty "true"

# ---- font zoom ------------------------------------------------------------
# Cmd-= and Cmd-- change the code size one point at a time, Cmd-0 goes back,
# and the size is written to the settings file under HOME. The UI font is not
# part of it: the sidebar keeps its width and rows.
mkdir -p "$T/zoomhome"
printf 'zoom me\n' > "$T/zoom.txt"
cat > "$T/zoom.script" <<SCRIPT
key 24 cmd =
dump $T/zoom-in.out
key 27 cmd -
key 27 cmd -
key 27 cmd -
dump $T/zoom-out.out
key 29 cmd 0
dump $T/zoom-reset.out
quit
SCRIPT
HOME="$T/zoomhome" CRC_SELFTEST="$T/zoom.script" "$BIN" "$T/zoom.txt" 2> "$T/zoom.err"
for pair in "zoom-in 14" "zoom-out 11" "zoom-reset 13"; do
    set -- $pair
    if ! grep -q "^layout: .* font $2 theme" "$T/$1.out"; then
        echo "FAIL $1.out: $(grep -m1 '^layout' "$T/$1.out"), expected font $2"
        fail=1
    fi
    if ! grep -q "^layout: window 1100x760 sidebar Some(240" "$T/$1.out"; then
        echo "FAIL $1.out: the sidebar should keep its width across a zoom"
        fail=1
    fi
done
if ! grep -q '^font_size = 13$' "$T/zoomhome/.config/crc/config.toml"; then
    echo "FAIL zoom: settings file not written: $(cat "$T/zoomhome/.config/crc/config.toml" 2>&1)"
    fail=1
fi

# ---- settings -------------------------------------------------------------
# Cmd-, opens the settings file as a tab, created from the template when
# there is none. Saving the tab applies it: here the size line is retyped.
mkdir -p "$T/settingshome"
printf 'plain\n' > "$T/settings.txt"
cat > "$T/settings.script" <<SCRIPT
key 43 cmd ,
dump $T/settings-open.out
key 1 cmd s
dump $T/settings-saved.out
quit
SCRIPT
HOME="$T/settingshome" CRC_SELFTEST="$T/settings.script" "$BIN" "$T/settings.txt" 2> "$T/settings.err"
if ! grep -q '^tabs: .*config.toml' "$T/settings-open.out"; then
    echo "FAIL settings-open.out: $(grep -m1 '^tabs' "$T/settings-open.out"), expected a config.toml tab"
    fail=1
fi
if ! grep -q '^font_size = 13$' "$T/settings-open.out"; then
    echo "FAIL settings-open.out: the template should be in the tab"
    fail=1
fi
if ! grep -q '^# crc settings' "$T/settingshome/.config/crc/config.toml"; then
    echo "FAIL settings: file not created from the template"
    fail=1
fi
expect "$T/settings-saved.out" dirty "false"

# ---- light appearance -----------------------------------------------------
# theme = "light" in the settings file picks the light table whatever the
# system says; "dark" the dark one. Both are checked, since the machine
# running this could be in either.
for choice in light dark; do
    mkdir -p "$T/theme-$choice/.config/crc"
    printf 'theme = "%s"\n' "$choice" > "$T/theme-$choice/.config/crc/config.toml"
    printf 'dump %s/theme-%s.out\nquit\n' "$T" "$choice" > "$T/theme-$choice.script"
    HOME="$T/theme-$choice" CRC_SELFTEST="$T/theme-$choice.script" "$BIN" "$T/settings.txt" 2> "$T/theme-$choice.err"
    if ! grep -q "^layout: .* theme $choice\$" "$T/theme-$choice.out"; then
        echo "FAIL theme-$choice.out: $(grep -m1 '^layout' "$T/theme-$choice.out"), expected theme $choice"
        fail=1
    fi
done

# ---- scrollbar ------------------------------------------------------------
# With the sidebar hidden the text pane is 1100x616 at y=116, so the track
# runs x=1091..1096, y=120..728. 401 lines in 32 rows put a 48 pt thumb at
# the top. Grabbing it 10 pt down and dragging 300 pt lands near line 198;
# the exact line depends on the row count, so a window is accepted.
seq 1 400 > "$T/scroll.txt"
cat > "$T/scroll.script" <<SCRIPT
key 11 cmd b
click 1093 400
dump $T/scroll-track.out
down 1093 130
drag 1093 430
up 1093 430
dump $T/scroll-drag.out
quit
SCRIPT
CRC_SELFTEST="$T/scroll.script" "$BIN" "$T/scroll.txt" 2> "$T/scroll.err"
# A press on the track brings the thumb there rather than placing the caret.
expect "$T/scroll-track.out" cursor "1:1"
track=$(grep -m1 '^scroll: ' "$T/scroll-track.out" | sed 's/scroll: //')
if [ "${track:-0}" -lt 150 ] || [ "${track:-0}" -gt 190 ]; then
    echo "FAIL scroll-track.out: scroll is [$track], expected about 170"
    fail=1
fi
drag=$(grep -m1 '^scroll: ' "$T/scroll-drag.out" | sed 's/scroll: //')
if [ "${drag:-0}" -lt 185 ] || [ "${drag:-0}" -gt 210 ]; then
    echo "FAIL scroll-drag.out: scroll is [$drag], expected about 198"
    fail=1
fi
expect "$T/scroll-drag.out" cursor "1:1"

# A trackpad moves the text by points: 30 pt is a line and a half at 19 pt
# rows. A click then lands on the line drawn under it, which is one further
# down than whole-line arithmetic would say.
cat > "$T/smooth.script" <<SCRIPT
key 11 cmd b
trackpad 600 400 -30
click 600 205
dump $T/smooth.out
quit
SCRIPT
CRC_SELFTEST="$T/smooth.script" "$BIN" "$T/scroll.txt" 2> "$T/smooth.err"
expect "$T/smooth.out" scroll "1"
expect "$T/smooth.out" scroll_fraction "0.58"
expect "$T/smooth.out" cursor "7:2"

# ---- big files --------------------------------------------------------------
# Past the read-only limit (lowered here: half a gigabyte per run is too
# much) the file opens, says why, and typing changes nothing.
printf 'first\nsecond\n' > "$T/big.txt"
cat > "$T/big.script" <<SCRIPT
text typed
key 36
dump $T/big.out
quit
SCRIPT
CRC_READ_ONLY_BYTES=8 CRC_SELFTEST="$T/big.script" "$BIN" "$T/big.txt" 2> "$T/big.err"
expect "$T/big.out" read_only "true"
expect "$T/big.out" dirty "false"
expect_line "$T/big.out" 1 "first"
expect_line "$T/big.out" 2 "second"
grep -q '^message: opened .*big.txt read-only: it is over 512 MB$' "$T/big.out" \
    || { echo "FAIL big.out: no read-only note: $(grep '^message:' "$T/big.out")"; fail=1; }

# Past the hard cap (sparse, so nothing is written) it is refused from its
# size alone, from inside the app; a test instance gets the status line
# instead of the alert. (On the command line crc prints the reason and exits.)
mkdir -p "$T/hugeproj"
mkfile -n 3g "$T/hugeproj/huge.txt"
cat > "$T/huge.script" <<SCRIPT
wait 300
key 35 cmd p
text huge
key 36
dump $T/huge.out
quit
SCRIPT
CRC_SELFTEST="$T/huge.script" "$BIN" "$T/hugeproj" 2> "$T/huge.err"
grep -q '^message: not opened: huge.txt is 3.0 GB; crc opens files up to 2.0 GB$' "$T/huge.out" \
    || { echo "FAIL huge.out: $(grep '^message:' "$T/huge.out")"; fail=1; }
rm -rf "$T/hugeproj"

# A non-ASCII line past the shaping limit stays editable, and the status
# line says it is drawn without shaping while it is on screen.
(yes 'é' || true) | head -c 3300000 | tr -d '\n' > "$T/long.txt"
cat > "$T/long.script" <<SCRIPT
wait 200
text x
dump $T/long.out
quit
SCRIPT
CRC_SELFTEST="$T/long.script" "$BIN" "$T/long.txt" 2> "$T/long.err"
expect "$T/long.out" read_only "false"
expect "$T/long.out" unshaped "true"
expect "$T/long.out" dirty "true"

# ---- ignored paths in the Explorer ------------------------------------------
# Git's ignored paths are read on a worker once the tree is indexed. Rows
# are sorted folders first: build (H: the ignored folder itself), src, then
# .gitignore, debug.log (H) and main.rs; I would be a row inside one.
mkdir -p "$T/ignproj/build" "$T/ignproj/src"
git -C "$T/ignproj" init -q
printf 'build/\n*.log\n' > "$T/ignproj/.gitignore"
: > "$T/ignproj/build/out.o"; : > "$T/ignproj/debug.log"; : > "$T/ignproj/main.rs"; : > "$T/ignproj/src/lib.rs"
# Then .gitignore is edited and saved in crc itself, which the watcher
# does not report (FSEvents skips our own writes): src must dim anyway.
cat > "$T/ign.script" <<SCRIPT
wait 800
wait 800
dump $T/ign.out
key 35 cmd p
text .gitignore
key 36
wait 300
key 125 cmd
text src/
key 1 cmd s
wait 800
wait 800
dump $T/ign-saved.out
quit
SCRIPT
CRC_SELFTEST="$T/ign.script" "$BIN" "$T/ignproj" 2> "$T/ign.err"
expect "$T/ign.out" ignored_rows "H--H-"
expect "$T/ign-saved.out" ignored_rows "HH-H-"

# ---- crash recovery ----------------------------------------------------------
# A real panic with a document unsaved: the hook writes it out as the
# process aborts, the next launch names it and restores it as unsaved
# changes, and the recovered copies are removed. The file on disk is not
# touched. Then the same with Discard.
mkdir -p "$T/crashproj"
printf 'fn main() {}\n' > "$T/crashproj/notes.rs"
recovery="$HOME/Library/Application Support/crc/recovery"
cat > "$T/crash.script" <<SCRIPT
text // kept
panic
SCRIPT
cat > "$T/restored.script" <<SCRIPT
dump $T/restored.out
quit
SCRIPT
CRC_SELFTEST="$T/crash.script" "$BIN" "$T/crashproj/notes.rs" 2> "$T/crash.log" || true
[ -n "$(ls -A "$recovery" 2>/dev/null)" ] \
    || { echo "FAIL crash: nothing written to $recovery"; fail=1; }
CRC_SELFTEST="$T/restored.script" "$BIN" 2> "$T/restored.log"
grep -q 'restore prompt: Unsaved changes to 1 document were written to disk as the app went down: notes.rs (in crashproj)\.' "$T/restored.log" \
    || { echo "FAIL restored.log: $(grep 'restore prompt' "$T/restored.log")"; fail=1; }
expect "$T/restored.out" tabs "notes.rs"
expect "$T/restored.out" dirty "true"
expect_line "$T/restored.out" 1 "// keptfn main() {}"
[ -z "$(ls -A "$recovery" 2>/dev/null)" ] \
    || { echo "FAIL restored: recovery copies left in $recovery"; fail=1; }
[ "$(cat "$T/crashproj/notes.rs")" = "fn main() {}" ] \
    || { echo "FAIL restored: the file on disk changed"; fail=1; }

# The crash left a log, and Help > Report a Problem (run from the palette)
# builds an issue from it. A test instance says what it would open.
grep -q '^panic: selftest: forced panic$' "$HOME/Library/Logs/crc/"crash-*.log \
    || { echo "FAIL crash: no crash log in $HOME/Library/Logs/crc"; fail=1; }
cat > "$T/report.script" <<SCRIPT
key 35 cmd p
text >report a problem
key 36
dump $T/report.out
quit
SCRIPT
CRC_SELFTEST="$T/report.script" "$BIN" "$T/crashproj/notes.rs" 2> "$T/report.err"
grep -q '^message: would open https://github.com/caioricciuti/crc/issues/new?body=What%20did%20you%20do%3F' "$T/report.out" \
    || { echo "FAIL report.out: $(grep '^message:' "$T/report.out" | cut -c1-160)"; fail=1; }
grep -q 'Last%20crash%3A%20panic%3A%20selftest%3A%20forced%20panic' "$T/report.out" \
    || { echo "FAIL report.out: the crash is not in the issue"; fail=1; }

# ---- update check -------------------------------------------------------------
# Help > Check for Updates, from the palette, against local listings: a newer
# release is announced (a test instance does not open the browser), and an
# empty listing means this is the latest.
printf '[{"tag_name": "v999.0.0", "html_url": "https://example.invalid/999", "draft": false}]' > "$T/releases-new.json"
printf '[]' > "$T/releases-none.json"
cat > "$T/update.script" <<SCRIPT
key 35 cmd p
text >check for updates
key 36
wait 500
wait 500
dump $T/update.out
quit
SCRIPT
CRC_UPDATE_URL="file://$T/releases-new.json" CRC_SELFTEST="$T/update.script" "$BIN" "$T/crashproj/notes.rs" 2> "$T/update.err"
expect "$T/update.out" message "crc 999.0.0 is out; would open https://example.invalid/999"
CRC_UPDATE_URL="file://$T/releases-none.json" CRC_SELFTEST="$T/update.script" "$BIN" "$T/crashproj/notes.rs" 2> "$T/update-none.err"
mv "$T/update.out" "$T/update-none.out"
grep -q '^message: crc .* is the latest release$' "$T/update-none.out" \
    || { echo "FAIL update-none.out: $(grep '^message:' "$T/update-none.out")"; fail=1; }

CRC_SELFTEST="$T/crash.script" "$BIN" "$T/crashproj/notes.rs" 2> "$T/crash2.log" || true
CRC_RESTORE_ANSWER=discard CRC_SELFTEST="$T/restored.script" "$BIN" "$T/crashproj/notes.rs" 2> "$T/discarded.log"
mv "$T/restored.out" "$T/discarded.out"
expect "$T/discarded.out" dirty "false"
expect_line "$T/discarded.out" 1 "fn main() {}"
# The crashes write their panic on purpose, so these logs are not *.err;
# past the prompt the relaunches must be as quiet as any other run.
if cat "$T/restored.log" "$T/discarded.log" | grep -v '^crc: restore prompt: ' | grep -v '^slow frame' | grep -q .; then
    echo "FAIL: a relaunch after a crash wrote to stderr:"
    cat "$T/restored.log" "$T/discarded.log"
    fail=1
fi
[ -z "$(ls -A "$recovery" 2>/dev/null)" ] \
    || { echo "FAIL discarded: recovery copies left in $recovery"; fail=1; }

# ---- mouse ----------------------------------------------------------------
printf 'abcdefghij\nklmnopqrst\nhello world foo\n\tTabbed line\nlast\n' > "$T/mouse.txt"
cat > "$T/mouse.script" <<SCRIPT
key 11 cmd b
click 51.2 144
dump $T/click.out
click 88 163 2
dump $T/double.out
click 60 125 3
dump $T/triple.out
click 300 62
dump $T/tabbar.out
click 24 125
down 24 125
drag 48 144
dump $T/drag.out
up 48 144
down 30 163 2
drag 125 163
up 125 163
dump $T/worddrag.out
click 24 125
down 300 62
drag 60 163
up 60 163
dump $T/strayDrag.out
click 88 163 2
text X
dump $T/replace.out
key 6 cmd z
dump $T/undo.out
quit
SCRIPT
CRC_SELFTEST="$T/mouse.script" "$BIN" "$T/mouse.txt" 2> "$T/mouse.err"
expect "$T/click.out" cursor "2:4"
expect "$T/double.out" selection '"world"'
expect "$T/triple.out" selection '"abcdefghij\n"'
expect "$T/tabbar.out" selection '"abcdefghij\n"'
expect "$T/drag.out" selection '"abcdefghij\nklm"'
expect "$T/worddrag.out" selection '"hello world foo"'
expect "$T/strayDrag.out" selection '""'
expect_line "$T/replace.out" 3 "hello X foo"
expect_line "$T/undo.out" 3 "hello world foo"
expect "$T/undo.out" selection '"world"'

# Long Unicode takes the shaped path, including real-window hit testing.
# Enough ASCII after the cluster to exceed the former 4096-byte limit.
printf 'e\314\201x ' > "$T/long-unicode.txt"
for ((i=0; i<600; i++)); do printf 'long text ' >> "$T/long-unicode.txt"; done
# RLI/PDI force the conservative full-paragraph caret path as well.
printf '\342\201\247שלום abc\342\201\251' >> "$T/long-unicode.txt"
cat > "$T/long-unicode.script" <<SCRIPT
key 11 cmd b
wait 300
click 32 125
key 123 shift
dump $T/long-unicode.out
text Q
wait 300
click 32 125
key 123 shift
dump $T/long-unicode-edited.out
key 6 cmd z
wait 300
click 32 125
key 123 shift
dump $T/long-unicode-restored.out
quit
SCRIPT
CRC_SELFTEST="$T/long-unicode.script" "$BIN" "$T/long-unicode.txt" 2> "$T/long-unicode.err"
expect "$T/long-unicode.out" selection '"e\u{301}"'
expect "$T/long-unicode.out" shaping_pending false
expect "$T/long-unicode-edited.out" selection '"Q"'
expect "$T/long-unicode-edited.out" shaping_pending false
expect "$T/long-unicode-restored.out" selection '"e\u{301}"'
expect "$T/long-unicode-restored.out" shaping_pending false

# Motion must also work deep in a long Unicode line while shaping is pending.
python3 - "$T/deep-motion.txt" <<'PYTHON'
import pathlib, sys
pathlib.Path(sys.argv[1]).write_text("é漢 " * 200000 + "e\u0301👩‍💻")
PYTHON
cat > "$T/deep-motion.script" <<SCRIPT
key 11 cmd b
key 124 cmd
key 123 shift
dump $T/deep-motion-emoji.out
key 51
key 123 shift
dump $T/deep-motion-accent.out
key 6 cmd z
dump $T/deep-motion-undo.out
quit
SCRIPT
CRC_SELFTEST="$T/deep-motion.script" "$BIN" "$T/deep-motion.txt" 2> "$T/deep-motion.err"
expect "$T/deep-motion-emoji.out" selection '"👩\u{200d}💻"'
expect "$T/deep-motion-accent.out" selection '"e\u{301}"'
expect "$T/deep-motion-undo.out" selection '"👩\u{200d}💻"'

# Full native shaping now also completes beyond the former 1 MiB cutoff.
python3 - "$T/large-shaped.txt" <<'PYTHON'
import pathlib, sys
pathlib.Path(sys.argv[1]).write_text("a" * 1200000 + "e\u0301👩‍💻\ntail")
PYTHON
cat > "$T/large-shaped.script" <<SCRIPT
key 11 cmd b
key 124 cmd
wait 1500
key 123 shift
dump $T/large-shaped.out
# Editing another line must keep the already-shaped paragraph ready.
key 124 cmd
key 125
text x
key 126
key 124 cmd
key 123 shift
dump $T/large-shaped-reused.out
quit
SCRIPT
CRC_SELFTEST="$T/large-shaped.script" "$BIN" "$T/large-shaped.txt" 2> "$T/large-shaped.err"
expect "$T/large-shaped.out" shaping_pending false
expect "$T/large-shaped.out" caret_shaped true
expect "$T/large-shaped.out" selection '"👩\u{200d}💻"'
expect "$T/large-shaped-reused.out" shaping_pending false
expect "$T/large-shaped-reused.out" caret_shaped true
expect "$T/large-shaped-reused.out" selection '"👩\u{200d}💻"'

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
click 300 280
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
    echo "FAIL kept.out: the text typed before opening another file is gone"
    fail=1
fi
if [ ! -s "$T/cursors.out" ]; then
    echo "FAIL cursors.out: no dump, the app did not survive undo with extra cursors"
    fail=1
fi

# Markdown opens as a preview; clicking it enters source editing. Tabs can
# then be dragged to a new position without changing which file is active.
printf '# One\n\nBody\n' > "$T/proj/one.md"
printf 'two\n' > "$T/proj/two.txt"
cat > "$T/tabs.script" <<SCRIPT
key 11 cmd b
dump $T/preview.out
click 100 160
dump $T/source.out
text X
dump $T/live-edit.out
key 35 cmd p
text two.txt
key 36
dump $T/two.out
down 50 62
drag 220 62
up 220 62
dump $T/reordered.out
click 700 62 2
dump $T/new-tab.out
quit
SCRIPT
CRC_SELFTEST="$T/tabs.script" "$BIN" "$T/proj/one.md" 2> "$T/tabs.err"
expect "$T/preview.out" preview "true"
expect "$T/source.out" preview "true"
expect "$T/source.out" live "true"
expect "$T/live-edit.out" preview "true"
expect_line "$T/live-edit.out" 1 "# OneX"
expect "$T/two.out" tabs "one.md | two.txt"
expect "$T/reordered.out" tabs "two.txt | one.md"
expect "$T/reordered.out" active "1"
expect "$T/reordered.out" preview "true"
expect "$T/new-tab.out" tabs "two.txt | one.md | Untitled"
expect "$T/new-tab.out" active "2"

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
    || { echo "FAIL project replace: b.txt is $(cat "$T/replproj/b.txt")"; fail=1; }
[ "$(cat "$T/replproj/a.txt")" = "$(printf 'needle one\nneedle two')" ] \
    || { echo "FAIL project replace: the open a.txt was written"; fail=1; }
[ "$(cat "$T/replproj/c.txt")" = "nothing here" ] \
    || { echo "FAIL project replace: c.txt changed"; fail=1; }

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
# Tree rows start below the 48pt toolbar and 70pt sidebar header, so row n
# is centred at 118 + 26n + 13.
click 70 131
wait 250
click 80 157
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
resize 420 450
dump $T/title-narrow.out
click 145 24
dump $T/title-menu.out
touch $T/personal-project-with-a-long-folder-name/after.txt
dump $T/title-stale.out
click 217 99
wait 500
dump $T/title-refreshed.out
key 35 cmd p
text after.txt
key 36
dump $T/title-opened.out
quit
SCRIPT
HOME="$T/titlehome" CRC_SELFTEST="$T/title-menu.script" "$BIN" "$T/personal-project-with-a-long-folder-name" 2> "$T/title-menu.err"
if ! grep -q '^layout: window 420x450 sidebar Some(240' "$T/title-narrow.out"; then
    echo 'FAIL: narrow native window lost its sidebar or did not resize'
    fail=1
fi
expect "$T/title-menu.out" project_menu_requested true
expect "$T/title-stale.out" finder_entries 1
expect "$T/title-refreshed.out" finder_entries 2
expect "$T/title-opened.out" tabs after.txt
expect "$T/title-opened.out" key_handler_draws 0
expect "$T/title-opened.out" window_title after.txt

# A sidebar drag must never replace an existing entry, including a dangling
# symlink (which Path::exists reports as absent). Then check that a clear
# destination still accepts the same native mouse gesture.
mkdir -p "$T/moveproj/holder" "$T/movehome"
printf 'personal sample\n' > "$T/moveproj/source.txt"
ln -s missing-target "$T/moveproj/holder/source.txt"
cat > "$T/move.script" <<SCRIPT
wait 400
down 60 157
drag 60 131
up 60 131
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
    expect "$T/preview-$kind.out" native_preview true
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
# Source control is docked in the 240pt sidebar. Header is 70pt below the
# 48pt toolbar, so the branch row is at 118, the message field at 146, the
# Commit button at 182 and the list at 220. One untracked file means a
# section heading at 220 and its row at 246, with the stage control inset
# 30pt from the trailing edge of the column.
click 220 259
wait 500
dump $T/git-stage.out
click 220 259
wait 500
dump $T/git-unstage.out
click 220 259
wait 500
# Click the row body, not the control: it selects and opens the diff.
click 60 259
wait 500
dump $T/git-diff.out
click 120 161
text Personal commit
dump $T/git-message.out
click 120 196
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
expect "$T/git-message.out" git_focus true
expect "$T/git-message.out" git_message 'Personal commit'
expect "$T/git-message.out" dirty false
expect "$T/git-commit.out" git_changes 0
expect "$T/git-commit.out" git_message ''
expect "$T/git-closed.out" git_open false
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
click 60 259
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

# ---- HTTP request from a .http file ----------------------------------------
# Nothing listens on port 1, so curl fails fast. What is under test is the
# native path: Cmd-Return in a request file opens a response tab, the worker
# answers, and the tab is filled in and left clean.
mkdir -p "$T/httpproj"
printf '### Closed port\nGET http://127.0.0.1:1/\n' > "$T/httpproj/probe.http"
cat > "$T/http.script" <<SCRIPT
key 36 cmd
wait 1500
wait 100
dump $T/http.out
click @response.segment.2
wait 200
dump $T/http-request.out
quit
SCRIPT
CRC_SELFTEST="$T/http.script" "$BIN" "$T/httpproj/probe.http" 2> "$T/http.err"
expect "$T/http.out" tabs "probe.http | GET Closed port"
expect "$T/http.out" active 1
expect "$T/http.out" preview false
expect "$T/http.out" dirty false
# The body segment shows curl's own error when nothing came back; the
# Request segment shows what was sent.
if ! sed -n '/^text:$/,$p' "$T/http.out" | sed -n 2p | grep -q '^curl: (7)'; then
    echo "FAIL http.out: expected curl's connection error as the response body"
    fail=1
fi
expect_line "$T/http-request.out" 1 "GET http://127.0.0.1:1/"

# ---- Cmd-Delete in the palette edits the query, not the tree ---------------
# The File menu's Move to Trash is Cmd-Delete. With the palette open and a
# sidebar row selected it used to fire on that row. The row is selected by
# clicking it first; the palette then gets Cmd-Delete and Option-Delete.
mkdir -p "$T/trashproj"
printf 'keep me\n' > "$T/trashproj/caio.txt"
printf 'GET http://127.0.0.1:1/\n' > "$T/trashproj/test.http"
cat > "$T/trash.script" <<SCRIPT
click @sidebar.row.0
wait 200
key 35 cmd p
wait 200
text te st
key 51 opt
dump $T/trash-word.out
key 51 cmd
wait 200
dump $T/trash.out
quit
SCRIPT
CRC_SELFTEST="$T/trash.script" "$BIN" "$T/trashproj" 2> "$T/trash.err"
expect "$T/trash-word.out" palette_query "te "
expect "$T/trash.out" palette_query ""
if [ ! -f "$T/trashproj/caio.txt" ]; then
    echo "FAIL: Cmd-Delete in the palette moved the selected file to Trash"
    fail=1
fi

# ---- Cmd-P > lists and runs menu commands -----------------------------------
# A leading > turns the file palette into the menu's commands. Return runs
# the chosen one; a query matching nothing runs nothing.
mkdir -p "$T/cmdproj"
printf 'keep me\n' > "$T/cmdproj/notes.txt"
cat > "$T/commands.script" <<SCRIPT
wait 300
key 35 cmd p
wait 200
text >commit
dump $T/commands-commit.out
key 53
key 35 cmd p
wait 200
text >sourc
dump $T/commands.out
key 36
wait 300
dump $T/commands-run.out
# All commands, more than fit: the wheel scrolls the list without moving
# the selection, and the arrow keys bring the list back to the selection.
key 35 cmd p
wait 200
text >
wheel -3
dump $T/commands-wheel.out
key 125
dump $T/commands-keys.out
key 53
key 35 cmd p
wait 200
text >zzqq
dump $T/commands-none.out
key 36
wait 200
dump $T/commands-none-run.out
quit
SCRIPT
CRC_SELFTEST="$T/commands.script" "$BIN" "$T/cmdproj" 2> "$T/commands.err"
expect "$T/commands-commit.out" palette_first "Commit…"
expect "$T/commands.out" palette_first "Source Control"
expect "$T/commands-run.out" git_open true
expect "$T/commands-run.out" palette_query ""
expect "$T/commands-none.out" palette_first ""
expect "$T/commands-wheel.out" palette_scroll 3
expect "$T/commands-wheel.out" palette_first "About crc"
# Row 1 is selected above the rows in view; the list moves to show it.
expect "$T/commands-keys.out" palette_scroll 1
# Cmd-P closed Source Control; running the toggle by mistake would reopen it.
expect "$T/commands-none-run.out" git_open false
expect "$T/commands-none-run.out" dirty false

# ---- @ and # in Cmd-P go to a symbol ----------------------------------------
# Cmd-R opens the palette on @: the active file's definitions, in file
# order while the query is empty. # searches the project index, which the
# indexer fills on its worker, so the script waits for it first.
mkdir -p "$T/symproj/src"
printf 'fn alpha() {}\n\nstruct Garden;\n\nfn water_beds() {}\n' > "$T/symproj/src/main.rs"
printf 'pub fn harvest_total() -> u32 { 0 }\n' > "$T/symproj/src/lib.rs"
cat > "$T/symbols.script" <<SCRIPT
wait 800
wait 800
key 15 cmd r
wait 200
dump $T/symbols-outline.out
text wat
dump $T/symbols-query.out
key 36
wait 200
dump $T/symbols-jump.out
key 17 cmd t
wait 200
text harv
dump $T/symbols-project.out
key 36
wait 300
dump $T/symbols-open.out
quit
SCRIPT
CRC_SELFTEST="$T/symbols.script" "$BIN" "$T/symproj/src/main.rs" 2> "$T/symbols.err"
expect "$T/symbols-outline.out" palette_query "@"
expect "$T/symbols-outline.out" palette_first "alpha"
expect "$T/symbols-query.out" palette_first "water_beds"
expect "$T/symbols-jump.out" cursor "5:1"
expect "$T/symbols-jump.out" palette_query ""
expect "$T/symbols-project.out" palette_first "harvest_total"
expect "$T/symbols-open.out" window_title "lib.rs"
expect "$T/symbols-open.out" cursor "1:1"

# ---- word wrap ---------------------------------------------------------------
# A .txt file wraps by default. The long second line is 264 columns; at this
# window's 103 the first row holds two sentences and "the quick brown " (104
# bytes, the space hanging), the second 103. Down moves by row, a click on the third screen
# row lands in the second row of line 2, and View > Word Wrap turns it off.
mkdir -p "$T/wrapproj"
python3 -c "import sys; w='the quick brown fox jumps over the lazy dog '; open(sys.argv[1],'w').write('short line\n'+w*6+'\nlast line\n')" "$T/wrapproj/notes.txt"
cat > "$T/wrap.script" <<SCRIPT
wait 300
dump $T/wrap-open.out
key 125 -
key 125 -
dump $T/wrap-down.out
key 125 -
dump $T/wrap-next.out
key 125 -
dump $T/wrap-last.out
click 298 163
dump $T/wrap-click.out
key 35 cmd p
wait 200
text >word wrap
key 36
wait 200
dump $T/wrap-off.out
quit
SCRIPT
CRC_SELFTEST="$T/wrap.script" "$BIN" "$T/wrapproj/notes.txt" 2> "$T/wrap.err"
expect "$T/wrap-open.out" wrap "103 row 0"
expect "$T/wrap-down.out" cursor "2:105"
expect "$T/wrap-next.out" cursor "2:208"
expect "$T/wrap-last.out" cursor "3:1"
expect "$T/wrap-click.out" cursor "2:109"
expect "$T/wrap-off.out" wrap "off row 0"
expect "$T/wrap-off.out" message "word wrap off"

# ---- folding -------------------------------------------------------------------
# The chevron after a line number folds the block below it by indentation;
# Down steps over what is hidden; the palette's Unfold All and Fold All
# commands open everything and fold every top-level block. Eleven lines, so
# the gutter is two digits (32 pt) and its chevron cell starts at 23 pt.
mkdir -p "$T/foldproj"
printf 'fn a() {\n    one;\n    two;\n}\n\ndef b():\n    x = 1\n\n    return x\nend\n' > "$T/foldproj/blocks.rs"
cat > "$T/fold.script" <<SCRIPT
wait 300
click 268 125
dump $T/fold-click.out
key 125 -
dump $T/fold-down.out
key 35 cmd p
wait 200
text >unfold all
key 36
wait 100
dump $T/fold-open.out
key 35 cmd p
wait 200
text >fold all
key 36
wait 100
dump $T/fold-all.out
quit
SCRIPT
CRC_SELFTEST="$T/fold.script" "$BIN" "$T/foldproj/blocks.rs" 2> "$T/fold.err"
expect "$T/fold-click.out" folds "2-3"
expect "$T/fold-down.out" cursor "4:1"
expect "$T/fold-open.out" folds ""
expect "$T/fold-all.out" folds "2-3 7-9"

# ---- indentation from .editorconfig -------------------------------------------
# Two spaces for *.ts: Tab on an empty line inserts two, Tab again two more,
# Shift-Tab takes one level back. A file with no editorconfig section and no
# indented lines keeps the tab character.
mkdir -p "$T/indentproj"
printf 'root = true\n[*.ts]\nindent_style = space\nindent_size = 2\n' > "$T/indentproj/.editorconfig"
printf 'let a = 1;\n' > "$T/indentproj/app.ts"
cat > "$T/indent.script" <<SCRIPT
wait 300
key 125 cmd
key 48 -
key 48 -
text x
dump $T/indent-tab.out
key 48 shift
dump $T/indent-back.out
quit
SCRIPT
CRC_SELFTEST="$T/indent.script" "$BIN" "$T/indentproj/app.ts" 2> "$T/indent.err"
expect_line "$T/indent-tab.out" 2 "    x"
expect_line "$T/indent-back.out" 2 "  x"

# ---- Cmd-Delete in the document edits the document, not the tree -----------
# The tree kept its selection after a file row was clicked, and that was
# enough to enable Move to Trash: Cmd-Delete while typing trashed the file.
# The file's row is clicked, which opens it; Cmd-Delete then goes to the
# document, straight away and again after typing.
mkdir -p "$T/docdelproj"
printf 'keep me\n' > "$T/docdelproj/a.txt"
cat > "$T/docdel.script" <<SCRIPT
click @sidebar.row.0
wait 300
key 51 cmd
wait 200
key 124
text xyz
key 51 cmd
wait 200
dump $T/docdel.out
quit
SCRIPT
CRC_SELFTEST="$T/docdel.script" "$BIN" "$T/docdelproj" 2> "$T/docdel.err"
expect "$T/docdel.out" tabs "a.txt"
expect_line "$T/docdel.out" 1 "eep me"
if [ ! -f "$T/docdelproj/a.txt" ]; then
    echo "FAIL: Cmd-Delete in the document moved the open file to Trash"
    fail=1
fi

# ---- the project watcher --------------------------------------------------
# A file created by another process shows up in the finder without a
# refresh. The scripted touch writes it from this process, which FSEvents
# would ignore as our own, so a subprocess writes it instead.
mkdir -p "$T/watchproj"
printf 'one\n' > "$T/watchproj/first.txt"
cat > "$T/watch.script" <<SCRIPT
wait 600
wait 100
dump $T/watch-before.out
wait 1200
wait 400
wait 400
wait 400
wait 400
dump $T/watch-after.out
quit
SCRIPT
( sleep 2.2; printf 'two\n' > "$T/watchproj/second.txt" ) &
CRC_SELFTEST="$T/watch.script" "$BIN" "$T/watchproj" 2> "$T/watch.err"
wait
expect "$T/watch-before.out" finder_entries 1
expect "$T/watch-after.out" finder_entries 2

# ---- split panes -----------------------------------------------------------
# Cmd-\ opens an empty pane to the right and focuses it; Cmd-Option-[ and ]
# move focus; a file open in another pane comes to the front there instead
# of opening twice; Cmd-Option-W closes the pane.
mkdir -p "$T/paneproj"
printf 'left\n' > "$T/paneproj/left.txt"
printf 'right\n' > "$T/paneproj/right.txt"
cat > "$T/pane.script" <<SCRIPT
wait 600
wait 100
key 42 cmd \\
wait 100
dump $T/pane-split.out
key 33 cmd,opt [
wait 100
dump $T/pane-left.out
key 30 cmd,opt ]
key 35 cmd p
text right
wait 300
key 36
wait 300
dump $T/pane-right.out
key 33 cmd,opt [
key 35 cmd p
text right
wait 300
key 36
wait 300
dump $T/pane-dedup.out
key 13 cmd,opt w
wait 100
dump $T/pane-closed.out
quit
SCRIPT
CRC_SELFTEST="$T/pane.script" "$BIN" "$T/paneproj/left.txt" 2> "$T/pane.err"
expect "$T/pane-split.out" panes 2
expect "$T/pane-split.out" focused_pane 1
expect "$T/pane-split.out" tabs "Home"
expect "$T/pane-left.out" focused_pane 0
expect "$T/pane-left.out" tabs "left.txt"
expect "$T/pane-right.out" focused_pane 1
expect "$T/pane-right.out" tabs "right.txt"
expect "$T/pane-dedup.out" focused_pane 1
expect "$T/pane-dedup.out" tabs "right.txt"
expect "$T/pane-closed.out" panes 1
expect "$T/pane-closed.out" tabs "left.txt"

# ---- language server --------------------------------------------------------
# The fake server in scripts/fake-lsp.py stands in for rust-analyzer: it
# flags TODO lines, completes alpha/alphabet/beta/gamma and defines
# everything at 1:1. Diagnostics arrive on open, the list follows typing,
# Tab accepts the suggestion, F12 jumps.
mkdir -p "$T/lspproj"
printf 'fn main() {\n    // TODO later\n}\n' > "$T/lspproj/main.rs"
cat > "$T/lsp.script" <<SCRIPT
wait 800
wait 400
wait 400
dump $T/lsp-open.out
text alp
wait 400
wait 300
dump $T/lsp-list.out
key 48
wait 200
dump $T/lsp-accepted.out
key 111
wait 400
wait 300
dump $T/lsp-definition.out
quit
SCRIPT
CRC_LSP_FAKE="$PWD/scripts/fake-lsp.py" CRC_SELFTEST="$T/lsp.script" "$BIN" "$T/lspproj/main.rs" 2> "$T/lsp.err"
expect "$T/lsp-open.out" lsp "fake Ready"
expect "$T/lsp-open.out" diagnostics 1
expect "$T/lsp-list.out" completion "alpha|alphabet"
expect_line "$T/lsp-accepted.out" 1 "alpha()fn main() {"
expect "$T/lsp-accepted.out" completion ""
expect "$T/lsp-definition.out" cursor "1:1"

# ---- language server: references, rename, format, signature help ----------
# The same fake server. Shift-F12 lists every use of the name under the
# caret in the find bar; F2 renames it in the open file and in one that is
# not open, on disk; Shift-Option-F formats (trailing spaces, `){`); typing
# `(` shows the signature and `,` moves to the second parameter.
mkdir -p "$T/lsp2proj"
printf 'fn total(){   \n    let count = 1;\n    count + count\n}\n' > "$T/lsp2proj/main.rs"
printf 'fn other() { count(); }\n' > "$T/lsp2proj/other.rs"
cat > "$T/lsp2.script" <<SCRIPT
wait 800
wait 400
wait 400
key 125 -
key 124 -
key 124 -
key 124 -
key 124 -
key 124 -
key 124 -
key 124 -
key 124 -
key 124 -
key 111 shift
wait 400
wait 300
dump $T/lsp2-refs.out
key 53
key 120 -
wait 100
text tally
dump $T/lsp2-field.out
key 36
wait 400
wait 300
dump $T/lsp2-renamed.out
key 3 shift,opt f
wait 400
wait 300
dump $T/lsp2-formatted.out
key 125 cmd
text alpha(1,
wait 400
wait 300
dump $T/lsp2-signature.out
text  2)
wait 300
dump $T/lsp2-closed.out
quit
SCRIPT
CRC_LSP_FAKE="$PWD/scripts/fake-lsp.py" CRC_SELFTEST="$T/lsp2.script" "$BIN" "$T/lsp2proj/main.rs" 2> "$T/lsp2.err"
expect "$T/lsp2-refs.out" find_results "main.rs:2|main.rs:3|main.rs:3|other.rs:1"
expect "$T/lsp2-refs.out" message "4 references"
expect "$T/lsp2-field.out" rename "tally"
expect_line "$T/lsp2-renamed.out" 2 "    let tally = 1;"
expect_line "$T/lsp2-renamed.out" 3 "    tally + tally"
expect "$T/lsp2-renamed.out" dirty true
expect "$T/lsp2-renamed.out" message "renamed in 2 files, 1 open and unsaved"
# other.rs was not open: renamed on disk. main.rs is open: not written.
[ "$(cat "$T/lsp2proj/other.rs")" = "fn other() { tally(); }" ] \
    || { echo "FAIL lsp2: other.rs is: $(cat "$T/lsp2proj/other.rs")"; fail=1; }
grep -q 'let count = 1;' "$T/lsp2proj/main.rs" \
    || { echo "FAIL lsp2: the open main.rs was written to disk"; fail=1; }
expect_line "$T/lsp2-formatted.out" 1 "fn total() {"
expect "$T/lsp2-formatted.out" message "formatted"
expect "$T/lsp2-signature.out" signature "fn alpha(first: i32, second: i32) [second: i32]"
expect "$T/lsp2-closed.out" signature ""

# ---- branches, remotes and blame --------------------------------------------
# A throwaway repository with a bare one as origin. The caret's line is
# blamed in the status line; Git > Switch Branch creates a branch from the
# palette; Pull with no upstream says so; Push sets the upstream on origin.
mkdir -p "$T/branchproj"
git init -q -b main "$T/branchproj"
git init -q --bare "$T/branchorigin.git"
printf 'one\ntwo\n' > "$T/branchproj/notes.txt"
git -C "$T/branchproj" add notes.txt
git -C "$T/branchproj" -c user.name=Tester -c user.email=t@example.com -c commit.gpgsign=false commit -q -m "first notes"
git -C "$T/branchproj" remote add origin "$T/branchorigin.git"
cat > "$T/branch.script" <<SCRIPT
wait 800
wait 600
wait 300
dump $T/branch-blame.out
key 35 cmd p
wait 200
text >switch branch
key 36
wait 200
text feature
dump $T/branch-picker.out
key 36
wait 600
wait 300
dump $T/branch-created.out
key 35 cmd p
wait 200
text >pull
key 36
wait 600
wait 300
dump $T/branch-pull.out
key 35 cmd p
wait 200
text >push
key 36
wait 1500
wait 300
dump $T/branch-push.out
quit
SCRIPT
CRC_SELFTEST="$T/branch.script" "$BIN" "$T/branchproj/notes.txt" 2> "$T/branch.err"
expect "$T/branch-blame.out" branch "main"
grep -q '^blame: Tester, .*: first notes$' "$T/branch-blame.out" \
    || { echo "FAIL branch-blame.out: $(grep '^blame:' "$T/branch-blame.out")"; fail=1; }
expect "$T/branch-picker.out" palette_first "Create branch “feature”"
expect "$T/branch-created.out" branch "feature"
expect "$T/branch-created.out" message "created and switched to feature"
expect "$T/branch-pull.out" message "this branch has no upstream to pull from"
expect "$T/branch-push.out" branch "feature"
grep -q '^message: pushed' "$T/branch-push.out" \
    || { echo "FAIL branch-push.out: $(grep '^message:' "$T/branch-push.out")"; fail=1; }
git -C "$T/branchorigin.git" rev-parse --verify -q refs/heads/feature > /dev/null \
    || { echo "FAIL branch push: origin has no feature branch"; fail=1; }

# ---- completion without a server ------------------------------------------
# Words from the file itself, a symbol from the project index, a path in a
# .gitignore with the Explorer previewing it, and history lifting a pick.
# No language server anywhere: these come from the worker and SQLite.
mkdir -p "$T/compproj/src" "$T/compproj/public"
printf 'water watering waterfall\n' > "$T/compproj/notes.txt"
printf 'pub fn harvest_total() -> u32 { 0 }\n' > "$T/compproj/src/lib.rs"
printf 'dist/\n' > "$T/compproj/.gitignore"
git -C "$T/compproj" init -q
cat > "$T/comp-words.script" <<SCRIPT
wait 800
wait 800
key 125 cmd
text wat
wait 300
wait 300
dump $T/comp-words.out
key 48
wait 200
dump $T/comp-words-accepted.out
text  harv
wait 300
wait 300
dump $T/comp-symbol.out
key 53
text  waterf
wait 300
wait 300
key 48
text  waterf
wait 300
wait 300
key 48
text  wat
wait 300
wait 300
dump $T/comp-history.out
quit
SCRIPT
CRC_SELFTEST="$T/comp-words.script" "$BIN" "$T/compproj/notes.txt" 2> "$T/comp-words.err"
expect "$T/comp-words.out" completion "water|watering|waterfall"
expect "$T/comp-words.out" completion_why "used once in this file"
expect_line "$T/comp-words-accepted.out" 2 "water"
expect "$T/comp-words-accepted.out" completion ""
expect "$T/comp-symbol.out" completion "harvest_total"
expect "$T/comp-symbol.out" completion_why "function in src/lib.rs:1"
# waterfall was picked twice; now it leads what `wat` offers.
grep -q '^completion: waterfall|' "$T/comp-history.out" \
    || { echo "FAIL comp-history.out: $(grep '^completion:' "$T/comp-history.out")"; fail=1; }
grep -q '^completion_why: you picked this 2× here' "$T/comp-history.out" \
    || { echo "FAIL comp-history.out: $(grep '^completion_why:' "$T/comp-history.out")"; fail=1; }

cat > "$T/comp-path.script" <<SCRIPT
wait 800
wait 800
key 125 cmd
text /pu
wait 300
wait 300
dump $T/comp-path.out
key 48
wait 300
dump $T/comp-path-accepted.out
quit
SCRIPT
CRC_SELFTEST="$T/comp-path.script" "$BIN" "$T/compproj/.gitignore" 2> "$T/comp-path.err"
expect "$T/comp-path.out" completion "public/"
expect "$T/comp-path.out" completion_why "folder"
# Rows: public, src, .gitignore, notes.txt; public previews as ignored.
expect "$T/comp-path.out" ignored_rows "P---"
expect_line "$T/comp-path-accepted.out" 2 "/public/"

# ---- inline new file in the sidebar ----------------------------------------
# New File is a name field in the tree, not a save panel. Return creates the
# file and opens it; Escape drops the field and creates nothing. Regions are
# clicked by name, so the layout may move without the script noticing.
mkdir -p "$T/newproj"
printf 'x\n' > "$T/newproj/existing.txt"
cat > "$T/newfile.script" <<SCRIPT
click @sidebar.action.0
wait 200
text draft.md
key 36
wait 800
dump $T/newfile.out
click @sidebar.action.0
wait 200
text ignored.txt
key 53
wait 200
dump $T/newfile-cancel.out
quit
SCRIPT
CRC_SELFTEST="$T/newfile.script" "$BIN" "$T/newproj" 2> "$T/newfile.err"
expect "$T/newfile.out" tabs "draft.md"
expect "$T/newfile.out" window_title "draft.md"
expect "$T/newfile.out" sidebar_edit ""
if [ ! -f "$T/newproj/draft.md" ]; then
    echo "FAIL: Return in the sidebar name field did not create the file"
    fail=1
fi
expect "$T/newfile-cancel.out" sidebar_edit ""
if [ -e "$T/newproj/ignored.txt" ]; then
    echo "FAIL: Escape in the sidebar name field created a file"
    fail=1
fi

# ---- Claude Code ------------------------------------------------------------
# scripts/fake-claude.py plays claude: it finds the lock file, is refused
# with a wrong token, runs the MCP handshake and the read-only tools, waits
# for the selection, then proposes two edits. The first is accepted with the
# button, which claude answers by writing the file; the second is rejected
# with Escape and must leave its file alone.
mkdir -p "$T/claudeproj"
printf 'fn main() {}\n' > "$T/claudeproj/main.txt"
printf 'one\ntwo\nthree\n' > "$T/claudeproj/accept.txt"
printf 'unchanged\n' > "$T/claudeproj/reject.txt"
python3 scripts/fake-claude.py "$CLAUDE_CONFIG_DIR" "$T/claudeproj" "$T/claude.json" 2> "$T/fake-claude.err" &
FAKE_CLAUDE=$!
cat > "$T/claude.script" <<SCRIPT
wait 500
wait 500
wait 500
wait 500
dump $T/claude-review.out
click @review.accept
wait 300
wait 300
wait 300
wait 300
dump $T/claude-second.out
key 53
wait 300
wait 300
wait 300
dump $T/claude-done.out
quit
SCRIPT
CRC_SELFTEST="$T/claude.script" "$BIN" "$T/claudeproj/main.txt" 2> "$T/claude.err"
wait "$FAKE_CLAUDE" || true
if [ -s "$T/fake-claude.err" ]; then
    echo "FAIL: fake-claude.py wrote to stderr:"
    cat "$T/fake-claude.err"
    fail=1
fi
# expect_claude <python expression over the log `j`> <description>
expect_claude() {
    if ! python3 -c "import json,sys; j=json.load(open('$T/claude.json')); sys.exit(0 if ($1) else 1)" 2>/dev/null; then
        echo "FAIL claude: $2"
        fail=1
    fi
}
expect_claude "not j['errors']" "fake claude reported errors: $(cat "$T/claude.json" 2>/dev/null)"
expect_claude "j['lock_mode'] == '0o600'" "lock file is not private"
expect_claude "j['lock_ide'] == 'crc' and j['lock_transport'] == 'ws'" "lock file names"
expect_claude "j['wrong_token'].startswith('HTTP/1.1 401')" "a wrong token was not refused"
expect_claude "j['right_token'].startswith('HTTP/1.1 101')" "the right token was not accepted"
expect_claude "j['protocol'] == '2024-11-05' and j['server'] == 'crc'" "initialize"
expect_claude "'openDiff' in j['tools'] and 'getDiagnostics' in j['tools']" "tools/list"
expect_claude "j['editors'] == ['main.txt']" "getOpenEditors"
expect_claude "j['selection_file'] == 'main.txt' and j['selection_empty']" "selection_changed"
expect_claude "j['selection_start'] == {'line': 0, 'character': 0}" "selection position"
expect_claude "j['accept_reply'] == ['FILE_SAVED', 'one\nTWO\nthree\n']" "accept reply"
expect_claude "j['accept_close'] == ['TAB_CLOSED']" "close_tab after accept"
expect_claude "j['reject_reply'] == ['DIFF_REJECTED', 'review-reject']" "reject reply"
expect_claude "j['reject_close'] == ['TAB_CLOSED']" "close_tab after reject"
expect "$T/claude-review.out" claude "connected=true reviews=1 pending=1"
expect "$T/claude-review.out" tabs "main.txt | ✻ accept.txt"
expect "$T/claude-second.out" claude "connected=true reviews=1 pending=1"
expect "$T/claude-second.out" tabs "main.txt | ✻ reject.txt"
expect "$T/claude-done.out" tabs "main.txt"
if [ "$(cat "$T/claudeproj/accept.txt")" != "$(printf 'one\nTWO\nthree')" ]; then
    echo "FAIL: the accepted change was not written"
    fail=1
fi
if [ "$(cat "$T/claudeproj/reject.txt")" != "unchanged" ]; then
    echo "FAIL: the rejected change touched its file"
    fail=1
fi
if ls "$CLAUDE_CONFIG_DIR/ide/"*.lock >/dev/null 2>&1; then
    echo "FAIL: a lock file outlived its app: $(ls "$CLAUDE_CONFIG_DIR/ide/")"
    fail=1
fi

# ---- terminal -----------------------------------------------------------------
# The toolbar's Terminal button opens a shell in a panel under the editor;
# typing runs a command there and leaves the document alone. The panel's top
# edge drags taller. Cmd-Shift-C starts a Claude Code tab, and every session
# is told this window's IDE port; CRC_CLAUDE_COMMAND stands in for claude
# and prints what it was given. Cmd-W closes a tab, and a program that exits
# closes its own, taking the panel with the last one.
mkdir -p "$T/termproj"
printf 'untouched\n' > "$T/termproj/notes.txt"
cat > "$T/term.script" <<SCRIPT
wait 500
click @toolbar.terminal
wait 700
wait 700
text echo abc def
key 36
wait 500
wait 500
dump $T/term-shell.out
down @terminal.divider
dragby 0 -100
upby 0 -100
wait 200
dump $T/term-resized.out
key 8 cmd,shift C
wait 700
wait 700
wait 700
dump $T/term-claude.out
key 13 cmd w
wait 300
dump $T/term-closed.out
text exit
key 36
wait 500
wait 500
dump $T/term-exit.out
key 50 ctrl \`
wait 700
wait 700
dump $T/term-again.out
quit
SCRIPT
CRC_TERMINAL_SHELL=/bin/sh CRC_CLAUDE_COMMAND='echo port=$CLAUDE_CODE_SSE_PORT ide=$ENABLE_IDE_INTEGRATION; sleep 30' \
    CRC_SELFTEST="$T/term.script" "$BIN" "$T/termproj/notes.txt" 2> "$T/term.err"
# expect_terminal <file> <fixed text the terminal line must contain>
expect_terminal() {
    if ! grep -m1 '^terminal: ' "$1" | grep -qF -- "$2"; then
        echo "FAIL $(basename "$1"): terminal line lacks [$2]: $(grep -m1 '^terminal: ' "$1")"
        fail=1
    fi
}
expect_terminal "$T/term-shell.out" "open=true focus=true height=320 tabs=sh "
expect_terminal "$T/term-shell.out" '\nabc def'
expect_line "$T/term-shell.out" 1 "untouched"
expect "$T/term-shell.out" dirty false
expect_terminal "$T/term-resized.out" "height=420 "
expect_terminal "$T/term-claude.out" "tabs=sh|✻ Claude "
if ! grep -m1 '^terminal: ' "$T/term-claude.out" | grep -qE 'port=[0-9]+ ide=true'; then
    echo "FAIL term-claude.out: the Claude session was not given the IDE port: $(grep -m1 '^terminal: ' "$T/term-claude.out")"
    fail=1
fi
expect_terminal "$T/term-closed.out" "tabs=sh "
expect_terminal "$T/term-exit.out" "open=false focus=false height=420 tabs= "
expect_terminal "$T/term-again.out" "open=true focus=true height=420 tabs=sh "

# ---- terminal selection and file references ---------------------------------
# A triple click selects a line of output; Cmd-click on a path:line printed
# in the terminal opens that file at that line. The output is on screen row
# 1 after `clear`, since row 0 holds the command.
printf 'one\ntwo\nthree\n' > "$T/termproj/second.txt"
cat > "$T/termsel.script" <<SCRIPT
wait 500
click @toolbar.terminal
wait 700
wait 700
text clear
key 36
wait 500
text echo second.txt:2
key 36
wait 500
wait 500
clickin @terminal 30 24 3
wait 200
dump $T/termsel-line.out
clickin @terminal 30 24 1 cmd
wait 500
wait 500
dump $T/termsel-open.out
quit
SCRIPT
CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/termsel.script" "$BIN" "$T/termproj/notes.txt" 2> "$T/termsel.err"
expect_terminal "$T/termsel-line.out" 'selection=Some("second.txt:2")'
expect "$T/termsel-open.out" tabs "notes.txt | second.txt"
expect "$T/termsel-open.out" cursor "2:1"
expect_terminal "$T/termsel-open.out" "focus=false"

# "slow frame" lines are the app's own latency log, and a machine busy
# compiling will produce them. Anything else on stderr is a finding.
if cat "$T"/*.err | grep -v '^slow frame' | grep -q .; then
    echo "FAIL: the app wrote to stderr:"
    cat "$T"/*.err | grep -v '^slow frame'
    fail=1
fi

[ "$fail" = 0 ] && echo "selftest: editing, project search, native previews, HTTP requests, Claude Code, the terminal and audit crash cases pass"
exit $fail
