# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh markdown-shaped.

# ---- Markdown hiding on a line CoreText shapes -------------------------------
# "é" puts the line through CoreText. Its stars are hidden off the caret's
# line, so a click just past the first drawn letter lands after the "c",
# not after the first star.
printf '**caf\xc3\xa9** fim\nx\n' > "$T/md-shaped.md"
cat > "$T/md-shaped.script" <<SCRIPT
key 11 cmd b
key 125 cmd
wait 300
click 76 125
key 123 shift
dump $T/md-shaped.out
quit
SCRIPT
CRC_SELFTEST="$T/md-shaped.script" "$BIN" "$T/md-shaped.md" 2> "$T/md-shaped.err"
expect "$T/md-shaped.out" selection '"c"'
expect "$T/md-shaped.out" cursor "1:3"
