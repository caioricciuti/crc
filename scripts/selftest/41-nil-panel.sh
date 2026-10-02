# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh nil-panel.

# ---- a Save panel that never comes (BUG-031) -------------------------------------
# AppKit once returned no Save panel, and Cmd-S on an untitled document
# aborted the app. CRC_NIL_PANEL makes the panels nil: Save As (the same
# path Cmd-S takes for an untitled document) and New File must leave the
# app up, keep the edit unsaved, and say why.
printf 'beans\n' > "$T/nilpanel.txt"
cat > "$T/nilpanel.script" <<SCRIPT
wait 500
text tomatoes
key 1 cmd,shift S
wait 300
dump $T/nilpanel-saveas.out
key 45 cmd n
wait 300
dump $T/nilpanel-new.out
quit
SCRIPT
CRC_NIL_PANEL=1 CRC_SELFTEST="$T/nilpanel.script" "$BIN" "$T/nilpanel.txt" 2> "$T/nilpanel.err"
[ -s "$T/nilpanel-new.out" ] || failed "nilpanel: the app did not live through the missing panels"
expect_line "$T/nilpanel-saveas.out" 1 "tomatoesbeans"
expect "$T/nilpanel-saveas.out" dirty true
expect "$T/nilpanel-saveas.out" message "macOS did not open the panel; nothing was saved or opened, try again"
expect "$T/nilpanel-new.out" message "macOS did not open the panel; nothing was saved or opened, try again"
[ "$(cat "$T/nilpanel.txt")" = "beans" ] || failed "nilpanel: the file changed on disk"
