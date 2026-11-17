# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh open-behind.

# ---- open behind a page ------------------------------------------------------
# With two panes and the focused pane's column covered by a page that takes
# it whole (Settings, MCP), opening a file gives the column
# back: the page closes as Escape would close it, and the tab is on screen
# rather than behind the page while the status line says it opened. The
# same when the file is already open in the other pane and only comes to
# the front there.
mkdir -p "$T/behindproj" "$T/behindhome"
printf 'a\n' > "$T/behindproj/a.txt"
printf 'b\n' > "$T/behindproj/b.txt"
cat > "$T/behind.script" <<SCRIPT
wait 600
key 42 cmd \\
wait 200
key 43 cmd ,
wait 300
dump $T/behind-settings.out
key 35 cmd p
wait 200
text b.txt
wait 300
key 36
wait 300
dump $T/behind-settings-opened.out
key 35 cmd p
wait 200
text >mcp servers
wait 300
key 36
wait 300
dump $T/behind-mcp.out
key 35 cmd p
wait 200
text a.txt
wait 300
key 36
wait 300
dump $T/behind-mcp-opened.out
quit
SCRIPT
HOME="$T/behindhome" CRC_SELFTEST="$T/behind.script" "$BIN" "$T/behindproj/a.txt" 2> "$T/behind.err"
# The new pane is focused and empty under the Settings page.
expect "$T/behind-settings.out" panes 2
expect "$T/behind-settings.out" focused_pane 1
expect "$T/behind-settings.out" tabs "Home"
grep -q '^settings_page: open ' "$T/behind-settings.out" \
    || failed "behind-settings.out: Cmd-, did not open the Settings page: $(grep '^settings_page:' "$T/behind-settings.out")"
# Cmd-P b.txt: the page gives the column back and the tab shows.
expect "$T/behind-settings-opened.out" focused_pane 1
expect "$T/behind-settings-opened.out" tabs "b.txt"
grep -q '^settings_page: closed ' "$T/behind-settings-opened.out" \
    || failed "behind-settings-opened.out: the Settings page stayed over the opened file: $(grep '^settings_page:' "$T/behind-settings-opened.out")"
# MCP, then Cmd-P a.txt, which is open in the other pane: focus moves
# there and the page closes, so the tab that came to the front is seen.
grep -q '^mcp_page: open ' "$T/behind-mcp.out" \
    || failed "behind-mcp.out: MCP Servers did not open the page: $(grep '^mcp_page:' "$T/behind-mcp.out")"
expect "$T/behind-mcp-opened.out" focused_pane 0
expect "$T/behind-mcp-opened.out" tabs "a.txt"
grep -q '^mcp_page: closed ' "$T/behind-mcp-opened.out" \
    || failed "behind-mcp-opened.out: the MCP page stayed over the file: $(grep '^mcp_page:' "$T/behind-mcp-opened.out")"
