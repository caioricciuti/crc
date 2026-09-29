# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh ignored-paths.

# ---- ignored paths in the Explorer ------------------------------------------
# Git's ignored paths are read on a worker once the tree is indexed. Rows
# are sorted folders first: build (H: the ignored folder itself), src, then
# .gitignore, debug.log (H) and main.rs; I would be a row inside one.
mkdir -p "$T/ignproj/build" "$T/ignproj/src"
git -C "$T/ignproj" init -q
printf 'build/\n*.log\n' > "$T/ignproj/.gitignore"
: > "$T/ignproj/build/out.o"; : > "$T/ignproj/debug.log"; : > "$T/ignproj/main.rs"; : > "$T/ignproj/src/lib.rs"
# Then .gitignore is edited and saved in crc itself, which the watcher
# does not report (FSEvents skips our own writes): src must dim anyway.
cat > "$T/ign.script" <<SCRIPT
wait 800
wait 800
dump $T/ign.out
key 35 cmd p
text .gitignore
key 36
wait 300
key 125 cmd
text src/
key 1 cmd s
wait 800
wait 800
dump $T/ign-saved.out
quit
SCRIPT
CRC_SELFTEST="$T/ign.script" "$BIN" "$T/ignproj" 2> "$T/ign.err"
expect "$T/ign.out" ignored_rows "H--H-"
expect "$T/ign-saved.out" ignored_rows "HH-H-"
