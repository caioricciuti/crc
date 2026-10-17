# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh home.

# ---- Home with no folder, and the Explorer beside it ------------------------
# A saved session names a folder with a long path as recent, so Home, on
# nothing, lists it under Recent with its path cut to fit the column. The
# Explorer beside Home shows the one empty state; its button asks for a
# folder, which a test instance reports in the status message instead of
# showing the panel. The frames are for the eye, at a narrow and at a wide
# window, and go to $SELFTEST_FRAMES when that is set.
deep="$T/a-long-folder-name-for-the-recent-row/another-long-folder-name/homeproj"
mkdir -p "$deep" "$T/homehome/.config/crc" "$T/homehome/Library/Application Support/crc"
printf 'theme = "dark"\n' > "$T/homehome/.config/crc/config.toml"
printf 'hello\n' > "$deep/notes.txt"
printf '# crc session. Safe to delete.\nrecent=%s\n' "$deep" \
    > "$T/homehome/Library/Application Support/crc/session"
cat > "$T/home.script" <<SCRIPT
resize 900 600
wait 800
frame $T/home-narrow.bmp
dump $T/home-narrow.out
resize 1440 900
wait 400
frame $T/home-wide.bmp
click @sidebar.open
wait 300
dump $T/home-open.out
quit
SCRIPT
HOME="$T/homehome" CRC_SELFTEST="$T/home.script" "$BIN" 2> "$T/home.err"
[ -z "${SELFTEST_FRAMES:-}" ] || cp -f "$T/home-narrow.bmp" "$T/home-wide.bmp" "$SELFTEST_FRAMES/"
expect "$T/home-narrow.out" message ""
expect "$T/home-open.out" message "would show the Open Folder panel"

# ---- Home scrolls when it is taller than the pane --------------------------
# Twelve recent folders in the session (it keeps eight) at 1100 by 600 run
# past the pane. The wheel over the column scrolls it, no further than
# the end, where the last row is on screen; the Home key goes back to the
# top. The frame at the end is for the eye.
mkdir -p "$T/scrollhome/.config/crc" "$T/scrollhome/Library/Application Support/crc"
printf 'theme = "dark"\n' > "$T/scrollhome/.config/crc/config.toml"
printf '# crc session. Safe to delete.\n' > "$T/scrollhome/Library/Application Support/crc/session"
for n in 01 02 03 04 05 06 07 08 09 10 11 12; do
    mkdir -p "$T/scrollproj/garden-$n"
    printf 'recent=%s\n' "$T/scrollproj/garden-$n" \
        >> "$T/scrollhome/Library/Application Support/crc/session"
done
cat > "$T/scroll.script" <<SCRIPT
resize 1100 600
wait 800
dump $T/scroll-top.out
wheelat @text 0.5 0.5 -2000
wait 300
dump $T/scroll-end.out
frame $T/scroll-end.bmp
key 115
wait 200
dump $T/scroll-home.out
quit
SCRIPT
HOME="$T/scrollhome" CRC_SELFTEST="$T/scroll.script" "$BIN" 2> "$T/scroll.err"
[ -z "${SELFTEST_FRAMES:-}" ] || cp -f "$T/scroll-end.bmp" "$SELFTEST_FRAMES/"
top=$(grep -m1 '^home_scroll: ' "$T/scroll-top.out" | sed 's/^home_scroll: //')
end=$(grep -m1 '^home_scroll: ' "$T/scroll-end.out" | sed 's/^home_scroll: //')
case "$top" in
    "0 of "[1-9]*) ;;
    *) failed "scroll-top.out: home_scroll is [$top], expected 0 of a positive height" ;;
esac
case "$end" in
    "0 of "*) failed "scroll-end.out: home_scroll is [$end], the wheel did not scroll Home" ;;
    *" of "*) [ "${end% of *}" = "${end#* of }" ] \
        || failed "scroll-end.out: home_scroll is [$end], expected the end" ;;
    *) failed "scroll-end.out: no home_scroll line" ;;
esac
expect "$T/scroll-home.out" home_scroll "0 of ${end#* of }"
