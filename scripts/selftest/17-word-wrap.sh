# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh word-wrap.

# ---- word wrap ---------------------------------------------------------------
# A .txt file wraps by default. The long second line is 264 columns; at this
# window's 98 (after the 44pt icon strip) the first row holds two sentences
# and "the quick " (98 bytes), the second starts at "brown", column 99. Down moves by row, a click on the third screen
# row lands in the second row of line 2, and View > Word Wrap turns it off.
mkdir -p "$T/wrapproj"
python3 -c "import sys; w='the quick brown fox jumps over the lazy dog '; open(sys.argv[1],'w').write('short line\n'+w*6+'\nlast line\n')" "$T/wrapproj/notes.txt"
cat > "$T/wrap.script" <<SCRIPT
wait 300
dump $T/wrap-open.out
key 125 -
key 125 -
dump $T/wrap-down.out
key 125 -
dump $T/wrap-next.out
key 125 -
dump $T/wrap-last.out
click 342 163
dump $T/wrap-click.out
key 35 cmd p
wait 200
text >word wrap
key 36
wait 200
dump $T/wrap-off.out
quit
SCRIPT
CRC_SELFTEST="$T/wrap.script" "$BIN" "$T/wrapproj/notes.txt" 2> "$T/wrap.err"
expect "$T/wrap-open.out" wrap "98 row 0"
expect "$T/wrap-down.out" cursor "2:99"
expect "$T/wrap-next.out" cursor "2:197"
expect "$T/wrap-last.out" cursor "3:1"
expect "$T/wrap-click.out" cursor "2:103"
expect "$T/wrap-off.out" wrap "off row 0"
expect "$T/wrap-off.out" message "word wrap off"
