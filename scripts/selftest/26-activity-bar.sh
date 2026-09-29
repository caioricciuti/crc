# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh activity-bar.

# ---- activity bar --------------------------------------------------------------
# The icon strip at the left: another panel's icon switches to it, the
# active one hides the sidebar and brings it back, and Extensions puts its
# list in the sidebar and its details in the editor column.
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
click @activity.extensions
wait 400
dump $T/act-ext.out
click @extensions.close
wait 100
dump $T/act-ext-list.out
click @activity.explorer
wait 100
dump $T/act-explorer.out
quit
SCRIPT
HOME="$T/acthome" CRC_SELFTEST="$T/act.script" "$BIN" "$T/actproj" 2> "$T/act.err"
expect "$T/act-start.out" activity "explorer sidebar=on"
expect "$T/act-scm.out" activity "source-control sidebar=on"
expect "$T/act-scm.out" git_open true
expect "$T/act-hidden.out" activity "source-control sidebar=off"
expect "$T/act-back.out" activity "source-control sidebar=on"
expect "$T/act-ext.out" activity "extensions sidebar=on"
grep -q '^extensions: open ' "$T/act-ext.out" || failed "act-ext: the details did not open"
grep -q '^extensions: list ' "$T/act-ext-list.out" || failed "act-ext-list: Close did not keep the list"
expect "$T/act-explorer.out" activity "explorer sidebar=on"
expect "$T/act-explorer.out" extensions "closed"
