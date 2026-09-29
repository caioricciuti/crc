# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh mouse.

# ---- mouse ----------------------------------------------------------------
printf 'abcdefghij\nklmnopqrst\nhello world foo\n\tTabbed line\nlast\n' > "$T/mouse.txt"
cat > "$T/mouse.script" <<SCRIPT
key 11 cmd b
click 95.2 144
dump $T/click.out
click 132 163 2
dump $T/double.out
click 104 125 3
dump $T/triple.out
click 344 62
dump $T/tabbar.out
click 68 125
down 68 125
drag 92 144
dump $T/drag.out
up 92 144
down 74 163 2
drag 169 163
up 169 163
dump $T/worddrag.out
click 68 125
down 344 62
drag 104 163
up 104 163
dump $T/strayDrag.out
click 132 163 2
text X
dump $T/replace.out
key 6 cmd z
dump $T/undo.out
quit
SCRIPT
CRC_SELFTEST="$T/mouse.script" "$BIN" "$T/mouse.txt" 2> "$T/mouse.err"
expect "$T/click.out" cursor "2:4"
expect "$T/double.out" selection '"world"'
expect "$T/triple.out" selection '"abcdefghij\n"'
expect "$T/tabbar.out" selection '"abcdefghij\n"'
expect "$T/drag.out" selection '"abcdefghij\nklm"'
expect "$T/worddrag.out" selection '"hello world foo"'
expect "$T/strayDrag.out" selection '""'
expect_line "$T/replace.out" 3 "hello X foo"
expect_line "$T/undo.out" 3 "hello world foo"
expect "$T/undo.out" selection '"world"'

# Long Unicode takes the shaped path, including real-window hit testing.
# Enough ASCII after the cluster to exceed the former 4096-byte limit.
printf 'e\314\201x ' > "$T/long-unicode.txt"
for ((i=0; i<600; i++)); do printf 'long text ' >> "$T/long-unicode.txt"; done
# RLI/PDI force the conservative full-paragraph caret path as well.
printf '\342\201\247שלום abc\342\201\251' >> "$T/long-unicode.txt"
cat > "$T/long-unicode.script" <<SCRIPT
key 11 cmd b
wait 300
click 76 125
key 123 shift
dump $T/long-unicode.out
text Q
wait 300
click 76 125
key 123 shift
dump $T/long-unicode-edited.out
key 6 cmd z
wait 300
click 76 125
key 123 shift
dump $T/long-unicode-restored.out
quit
SCRIPT
CRC_SELFTEST="$T/long-unicode.script" "$BIN" "$T/long-unicode.txt" 2> "$T/long-unicode.err"
expect "$T/long-unicode.out" selection '"e\u{301}"'
expect "$T/long-unicode.out" shaping_pending false
expect "$T/long-unicode-edited.out" selection '"Q"'
expect "$T/long-unicode-edited.out" shaping_pending false
expect "$T/long-unicode-restored.out" selection '"e\u{301}"'
expect "$T/long-unicode-restored.out" shaping_pending false

# Motion must also work deep in a long Unicode line while shaping is pending.
python3 - "$T/deep-motion.txt" <<'PYTHON'
import pathlib, sys
pathlib.Path(sys.argv[1]).write_text("é漢 " * 200000 + "e\u0301👩‍💻")
PYTHON
cat > "$T/deep-motion.script" <<SCRIPT
key 11 cmd b
key 124 cmd
key 123 shift
dump $T/deep-motion-emoji.out
key 51
key 123 shift
dump $T/deep-motion-accent.out
key 6 cmd z
dump $T/deep-motion-undo.out
quit
SCRIPT
CRC_SELFTEST="$T/deep-motion.script" "$BIN" "$T/deep-motion.txt" 2> "$T/deep-motion.err"
expect "$T/deep-motion-emoji.out" selection '"👩\u{200d}💻"'
expect "$T/deep-motion-accent.out" selection '"e\u{301}"'
expect "$T/deep-motion-undo.out" selection '"👩\u{200d}💻"'

# Full native shaping now also completes beyond the former 1 MiB cutoff.
python3 - "$T/large-shaped.txt" <<'PYTHON'
import pathlib, sys
pathlib.Path(sys.argv[1]).write_text("a" * 1200000 + "e\u0301👩‍💻\ntail")
PYTHON
cat > "$T/large-shaped.script" <<SCRIPT
key 11 cmd b
key 124 cmd
wait 1500
key 123 shift
dump $T/large-shaped.out
# Editing another line must keep the already-shaped paragraph ready.
key 124 cmd
key 125
text x
key 126
key 124 cmd
key 123 shift
dump $T/large-shaped-reused.out
quit
SCRIPT
CRC_SELFTEST="$T/large-shaped.script" "$BIN" "$T/large-shaped.txt" 2> "$T/large-shaped.err"
expect "$T/large-shaped.out" shaping_pending false
expect "$T/large-shaped.out" caret_shaped true
expect "$T/large-shaped.out" selection '"👩\u{200d}💻"'
expect "$T/large-shaped-reused.out" shaping_pending false
expect "$T/large-shaped-reused.out" caret_shaped true
expect "$T/large-shaped-reused.out" selection '"👩\u{200d}💻"'
