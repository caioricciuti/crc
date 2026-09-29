# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh editorconfig.

# ---- indentation from .editorconfig -------------------------------------------
# Two spaces for *.ts: Tab on an empty line inserts two, Tab again two more,
# Shift-Tab takes one level back. A file with no editorconfig section and no
# indented lines keeps the tab character.
mkdir -p "$T/indentproj"
printf 'root = true\n[*.ts]\nindent_style = space\nindent_size = 2\n' > "$T/indentproj/.editorconfig"
printf 'let a = 1;\n' > "$T/indentproj/app.ts"
cat > "$T/indent.script" <<SCRIPT
wait 300
key 125 cmd
key 48 -
key 48 -
text x
dump $T/indent-tab.out
key 48 shift
dump $T/indent-back.out
quit
SCRIPT
CRC_SELFTEST="$T/indent.script" "$BIN" "$T/indentproj/app.ts" 2> "$T/indent.err"
expect_line "$T/indent-tab.out" 2 "    x"
expect_line "$T/indent-back.out" 2 "  x"
