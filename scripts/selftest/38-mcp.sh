# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh mcp.

# ---- MCP servers ------------------------------------------------------------------
# The fake server in scripts/fake-mcp.py stands in for a real one. View >
# MCP Servers lists mcp.json without starting anything; a click starts a
# server and lists its tools; a launcher that downloads code is refused; a
# tool opens a call document that Cmd-Return runs into an answer tab; a
# destructive tool asks first; a resource opens at once.
mkdir -p "$T/mcphome/.config/crc" "$T/mcpproj"
printf 'notes\n' > "$T/mcpproj/notes.txt"
cat > "$T/mcphome/.config/crc/mcp.json" <<JSON
{"mcpServers": {
  "garden": {"command": "/usr/bin/python3", "args": ["$PWD/scripts/fake-mcp.py", "legacy"]},
  "fetcher": {"command": "npx", "args": ["-y", "some-mcp-server"]}
}}
JSON
cat > "$T/mcp.script" <<SCRIPT
wait 800
wait 300
key 35 cmd p
wait 200
text >mcp servers
key 36
wait 300
dump $T/mcp-listed.out
click @mcp.server.garden
wait 800
wait 600
wait 300
dump $T/mcp-started.out
click @mcp.server.fetcher
wait 300
dump $T/mcp-refused.out
click @mcp.tool.garden.add
wait 300
dump $T/mcp-calldoc.out
key 36 cmd
wait 600
wait 300
dump $T/mcp-answer.out
click @mcp.tool.garden.wipe
wait 300
key 36 cmd
wait 600
wait 300
dump $T/mcp-wipe.out
click @mcp.resource.garden.0
wait 600
wait 300
dump $T/mcp-resource.out
quit
SCRIPT
HOME="$T/mcphome" CRC_SELFTEST="$T/mcp.script" "$BIN" "$T/mcpproj" 2> "$T/mcp.err"
grep -q '^activity: mcp ' "$T/mcp-listed.out" \
    || failed "mcp-listed.out: $(grep '^activity:' "$T/mcp-listed.out")"
# Listed, and nothing started.
expect "$T/mcp-listed.out" mcp "garden:stopped:0,fetcher:stopped:0"
expect "$T/mcp-started.out" mcp "garden:ready:3,fetcher:stopped:0"
grep -q '^mcp: garden:ready:3,fetcher:failed: npx downloads a package and runs it' "$T/mcp-refused.out" \
    || failed "mcp-refused.out: $(grep '^mcp:' "$T/mcp-refused.out")"
grep -q '^message: fetcher: npx downloads a package and runs it' "$T/mcp-refused.out" \
    || failed "mcp-refused.out: $(grep '^message:' "$T/mcp-refused.out")"
# The call document names the server and tool, with placeholders.
grep -q '^tabs: .*garden · add call' "$T/mcp-calldoc.out" \
    || failed "mcp-calldoc.out: $(grep '^tabs:' "$T/mcp-calldoc.out")"
expect_line "$T/mcp-calldoc.out" 2 '  "server": "garden",'
# Cmd-Return ran it: 0 + 0 in the answer tab.
expect "$T/mcp-answer.out" message "garden · add: done"
expect_line "$T/mcp-answer.out" 1 "0"
# The destructive tool asked, the test said no, and it did not run.
grep -q '^crc: mcp confirm prompt: garden · wipe: ' "$T/mcp.err" \
    || failed "mcp: running a destructive tool did not ask first"
# An answer tab is "garden · wipe" exactly; the call document's ends in "call".
grep -Eq '^tabs: (.* \| )?garden · wipe( \||$)' "$T/mcp-wipe.out" \
    && failed "mcp-wipe.out: the destructive tool ran without a yes"
grep -v '^crc: mcp confirm prompt: ' "$T/mcp.err" > "$T/mcp.err.rest" || true
mv "$T/mcp.err.rest" "$T/mcp.err"
expect_line "$T/mcp-resource.out" 1 "# Beds"

# ---- saved calls ------------------------------------------------------------------
# Run > Save MCP Call to Workspace writes the call into calls/; Home lists
# it, and a click runs it once the server is up.
mkdir -p "$T/mcpws/app"
git init -q "$T/mcpws/app"
cat > "$T/mcp-save.script" <<SCRIPT
wait 800
wait 300
key 35 cmd p
wait 200
text >mcp servers
key 36
wait 300
click @mcp.server.garden
wait 800
wait 600
wait 300
click @mcp.tool.garden.add
wait 300
key 35 cmd p
wait 200
text >save mcp call
key 36
wait 600
wait 300
dump $T/mcp-saved.out
quit
SCRIPT
HOME="$T/mcphome" CRC_SELFTEST="$T/mcp-save.script" "$BIN" "$T/mcpws" 2> "$T/mcp-save.err"
expect "$T/mcp-saved.out" message "saved as calls/garden-add.json; Home lists it"
[ -f "$T/mcpws/calls/garden-add.json" ] || failed "mcp-save: calls/garden-add.json was not written"
grep -q '^home: .* calls=1 ' "$T/mcp-saved.out" \
    || failed "mcp-saved.out: Home does not list the saved call: $(grep '^home:' "$T/mcp-saved.out")"

# ---- the page ------------------------------------------------------------------
# Opening MCP Servers takes the editor column with the page. Close gives the
# column back; a card shows a server's details without starting it; Add
# Server writes an entry for an installed program (the panel's choice comes
# from CRC_MCP_ADD here) and Add by URL takes an address in the palette;
# Open mcp.json shows the file, and saving it in crc reads it again.
mkdir -p "$T/mcphome2/.config/crc" "$T/mcpproj2"
cat > "$T/mcphome2/.config/crc/mcp.json" <<JSON
{"mcpServers": {
  "garden": {"command": "/usr/bin/python3", "args": ["$PWD/scripts/fake-mcp.py", "legacy"]}
}}
JSON
cat > "$T/mcp-page.script" <<SCRIPT
wait 800
wait 300
key 35 cmd p
wait 200
text >mcp servers
key 36
wait 300
dump $T/mcp-page-open.out
click @mcp.page.close
wait 300
dump $T/mcp-page-closed.out
click @mcp.select.garden
wait 300
dump $T/mcp-page-selected.out
click @mcp.page.add
wait 300
dump $T/mcp-page-added.out
click @mcp.page.add-url
wait 300
text example.com/mcp
key 36
wait 300
dump $T/mcp-page-url.out
click @mcp.page.home
wait 300
click @mcp.page.edit
wait 300
dump $T/mcp-page-edit.out
key 0 cmd a
text {"mcpServers": {"only": {"url": "https://only.example/mcp"}}}
key 1 cmd s
wait 600
wait 300
dump $T/mcp-page-saved.out
quit
SCRIPT
HOME="$T/mcphome2" CRC_MCP_ADD="/bin/cat" CRC_SELFTEST="$T/mcp-page.script" "$BIN" "$T/mcpproj2" 2> "$T/mcp-page.err"
grep -q '^mcp_page: open selected=- note=- ' "$T/mcp-page-open.out" \
    || failed "mcp-page-open.out: the page did not take the column: $(grep '^mcp_page:' "$T/mcp-page-open.out")"
grep -q '^mcp_page: closed ' "$T/mcp-page-closed.out" \
    || failed "mcp-page-closed.out: Close did not give the column back: $(grep '^mcp_page:' "$T/mcp-page-closed.out")"
grep -q '^mcp_page: open selected=garden note=- ' "$T/mcp-page-selected.out" \
    || failed "mcp-page-selected.out: the card did not open the details: $(grep '^mcp_page:' "$T/mcp-page-selected.out")"
expect "$T/mcp-page-selected.out" mcp "garden:stopped:0"
expect "$T/mcp-page-added.out" mcp "garden:stopped:0,cat:stopped:0"
grep -q '^mcp_page: open selected=cat note=Added cat to mcp.json: Start runs it ' "$T/mcp-page-added.out" \
    || failed "mcp-page-added.out: $(grep '^mcp_page:' "$T/mcp-page-added.out")"
expect "$T/mcp-page-url.out" mcp "garden:stopped:0,cat:stopped:0,example.com:stopped:0"
# The file opened in the tab holds what Add Server and Add by URL wrote.
expect_line "$T/mcp-page-edit.out" 11 '      "command": "/bin/cat"'
expect_line "$T/mcp-page-edit.out" 14 '      "url": "https://example.com/mcp"'
grep -q '^mcp_page: closed ' "$T/mcp-page-edit.out" \
    || failed "mcp-page-edit.out: Open mcp.json left the page over the file: $(grep '^mcp_page:' "$T/mcp-page-edit.out")"
grep -q '^tabs: .*mcp.json' "$T/mcp-page-edit.out" \
    || failed "mcp-page-edit.out: $(grep '^tabs:' "$T/mcp-page-edit.out")"
expect "$T/mcp-page-saved.out" mcp "only:stopped:0"
expect "$T/mcp-page-saved.out" message "mcp.json saved and read: 1 server"
