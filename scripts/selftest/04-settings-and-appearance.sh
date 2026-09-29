# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh settings-and-appearance.

# ---- settings -------------------------------------------------------------
# Cmd-, opens the settings file as a tab, created from the template when
# there is none. Saving the tab applies it: here the size line is retyped.
mkdir -p "$T/settingshome"
printf 'plain\n' > "$T/settings.txt"
cat > "$T/settings.script" <<SCRIPT
key 43 cmd ,
dump $T/settings-open.out
key 1 cmd s
dump $T/settings-saved.out
quit
SCRIPT
HOME="$T/settingshome" CRC_SELFTEST="$T/settings.script" "$BIN" "$T/settings.txt" 2> "$T/settings.err"
if ! grep -q '^tabs: .*config.toml' "$T/settings-open.out"; then
    echo "FAIL settings-open.out: $(grep -m1 '^tabs' "$T/settings-open.out"), expected a config.toml tab"
    fail=1
fi
if ! grep -q '^font_size = 13$' "$T/settings-open.out"; then
    echo "FAIL settings-open.out: the template should be in the tab"
    fail=1
fi
if ! grep -q '^# crc settings' "$T/settingshome/.config/crc/config.toml"; then
    echo "FAIL settings: file not created from the template"
    fail=1
fi
expect "$T/settings-saved.out" dirty "false"

# ---- light appearance -----------------------------------------------------
# theme = "light" in the settings file picks the light table whatever the
# system says; "dark" the dark one. Both are checked, since the machine
# running this could be in either.
for choice in light dark; do
    mkdir -p "$T/theme-$choice/.config/crc"
    printf 'theme = "%s"\n' "$choice" > "$T/theme-$choice/.config/crc/config.toml"
    printf 'dump %s/theme-%s.out\nquit\n' "$T" "$choice" > "$T/theme-$choice.script"
    HOME="$T/theme-$choice" CRC_SELFTEST="$T/theme-$choice.script" "$BIN" "$T/settings.txt" 2> "$T/theme-$choice.err"
    if ! grep -q "^layout: .* theme $choice\$" "$T/theme-$choice.out"; then
        echo "FAIL theme-$choice.out: $(grep -m1 '^layout' "$T/theme-$choice.out"), expected theme $choice"
        fail=1
    fi
done
