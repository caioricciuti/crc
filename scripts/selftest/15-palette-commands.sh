# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh palette-commands.

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
# Source Control stays open under the palette; running the toggle by
# mistake on a query with no matches would close it.
expect "$T/commands-none-run.out" git_open true
expect "$T/commands-none-run.out" dirty false
