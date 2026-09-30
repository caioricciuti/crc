# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh find-then-edit.

# ---- a click in the text while the find bar is open -------------------------
# The bar used to keep the keys: the click moved the caret, and typing still
# went into the query. Now the click gives the document the keys and leaves
# the bar open; Cmd-F gives them back, and Escape closes the bar.
for ((i=1; i<=30; i++)); do printf 'line %s\n' "$i" >> "$T/find-edit.txt"; done
cat > "$T/find-edit.script" <<SCRIPT
key 3 cmd f
text line 2
click 344 400
key 126 cmd
text Z
dump $T/find-edit-typed.out
key 3 cmd f
text q
dump $T/find-edit-refocused.out
key 53
key 126 cmd
text Y
dump $T/find-edit-closed.out
quit
SCRIPT
CRC_SELFTEST="$T/find-edit.script" "$BIN" "$T/find-edit.txt" 2> "$T/find-edit.err"
expect_line "$T/find-edit-typed.out" 1 "Zline 1"
expect_line "$T/find-edit-refocused.out" 1 "Zline 1"
expect_line "$T/find-edit-closed.out" 1 "YZline 1"
