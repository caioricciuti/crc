# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh folding.

# ---- folding -------------------------------------------------------------------
# The chevron after a line number folds the block below it by indentation;
# Down steps over what is hidden; the palette's Unfold All and Fold All
# commands open everything and fold every top-level block. Eleven lines, so
# the gutter is two digits (32 pt) and its chevron cell starts at 23 pt.
mkdir -p "$T/foldproj"
printf 'fn a() {\n    one;\n    two;\n}\n\ndef b():\n    x = 1\n\n    return x\nend\n' > "$T/foldproj/blocks.rs"
cat > "$T/fold.script" <<SCRIPT
wait 300
click 312 125
dump $T/fold-click.out
key 125 -
dump $T/fold-down.out
key 35 cmd p
wait 200
text >unfold all
key 36
wait 100
dump $T/fold-open.out
key 35 cmd p
wait 200
text >fold all
key 36
wait 100
dump $T/fold-all.out
quit
SCRIPT
CRC_SELFTEST="$T/fold.script" "$BIN" "$T/foldproj/blocks.rs" 2> "$T/fold.err"
expect "$T/fold-click.out" folds "2-3"
expect "$T/fold-down.out" cursor "4:1"
expect "$T/fold-open.out" folds ""
expect "$T/fold-all.out" folds "2-3 7-9"
