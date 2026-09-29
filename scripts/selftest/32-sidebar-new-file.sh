# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh sidebar-new-file.

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
