# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh font-zoom.

# ---- font zoom ------------------------------------------------------------
# Cmd-= and Cmd-- change the code size one point at a time, Cmd-0 goes back,
# and the size is written to the settings file under HOME. The UI font is not
# part of it: the sidebar keeps its width and rows.
mkdir -p "$T/zoomhome"
printf 'zoom me\n' > "$T/zoom.txt"
cat > "$T/zoom.script" <<SCRIPT
key 24 cmd =
dump $T/zoom-in.out
key 27 cmd -
key 27 cmd -
key 27 cmd -
dump $T/zoom-out.out
key 29 cmd 0
dump $T/zoom-reset.out
quit
SCRIPT
HOME="$T/zoomhome" CRC_SELFTEST="$T/zoom.script" "$BIN" "$T/zoom.txt" 2> "$T/zoom.err"
for pair in "zoom-in 14" "zoom-out 11" "zoom-reset 13"; do
    set -- $pair
    if ! grep -q "^layout: .* font $2 theme" "$T/$1.out"; then
        echo "FAIL $1.out: $(grep -m1 '^layout' "$T/$1.out"), expected font $2"
        fail=1
    fi
    if ! grep -q "^layout: window 1100x760 sidebar Some(240" "$T/$1.out"; then
        echo "FAIL $1.out: the sidebar should keep its width across a zoom"
        fail=1
    fi
done
if ! grep -q '^font_size = 13$' "$T/zoomhome/.config/crc/config.toml"; then
    echo "FAIL zoom: settings file not written: $(cat "$T/zoomhome/.config/crc/config.toml" 2>&1)"
    fail=1
fi
