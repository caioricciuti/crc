# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh activity-bar.

# ---- activity bar --------------------------------------------------------------
# The icon strip at the left: another panel's icon switches to it, the
# active one hides the sidebar and brings it back, and MCP puts its list
# in the sidebar and its page in the editor column.
mkdir -p "$T/actproj" "$T/acthome"
git init -q -b main "$T/actproj"
printf 'one\n' > "$T/actproj/a.txt"
git -C "$T/actproj" add a.txt
testgit "$T/actproj" commit -q -m first
printf 'two\n' >> "$T/actproj/a.txt"
cat > "$T/act.script" <<SCRIPT
wait 500
dump $T/act-start.out
click @activity.source-control
wait 300
dump $T/act-scm.out
click @activity.source-control
wait 100
dump $T/act-hidden.out
click @activity.source-control
wait 100
dump $T/act-back.out
click @activity.mcp
wait 400
dump $T/act-mcp.out
click @mcp.page.close
wait 100
dump $T/act-mcp-list.out
click @activity.explorer
wait 100
dump $T/act-explorer.out
click @activity.search
wait 100
dump $T/act-search.out
click @activity.explorer
wait 100
dump $T/act-search-left.out
quit
SCRIPT
HOME="$T/acthome" CRC_SELFTEST="$T/act.script" "$BIN" "$T/actproj" 2> "$T/act.err"
expect "$T/act-start.out" activity "explorer sidebar=on"
expect "$T/act-scm.out" activity "source-control sidebar=on"
expect "$T/act-scm.out" git_open true
expect "$T/act-hidden.out" activity "source-control sidebar=off"
expect "$T/act-back.out" activity "source-control sidebar=on"
expect "$T/act-mcp.out" activity "mcp sidebar=on"
grep -q '^mcp_page: open ' "$T/act-mcp.out" || failed "act-mcp: the page did not open"
grep -q '^mcp_page: closed ' "$T/act-mcp-list.out" || failed "act-mcp-list: Close did not give the column back"
expect "$T/act-mcp-list.out" activity "mcp sidebar=on"
expect "$T/act-explorer.out" activity "explorer sidebar=on"
# The Search view has its own icon, between Explorer and Source Control.
expect "$T/act-search.out" activity "search sidebar=on"
expect "$T/act-search-left.out" activity "explorer sidebar=on"
