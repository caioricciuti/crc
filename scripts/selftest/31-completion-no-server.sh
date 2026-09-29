# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh completion-no-server.

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
