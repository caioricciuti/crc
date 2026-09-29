# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh palette-symbols.

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
