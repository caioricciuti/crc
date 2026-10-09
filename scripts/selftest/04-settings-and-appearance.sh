# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh settings-and-appearance.

# ---- settings -------------------------------------------------------------
# Cmd-, opens the Settings page in the editor column; Escape gives the
# column back. Edit on a setting opens config.toml, created from the
# template when there is none, with the caret on that key's line. Saving
# the tab applies it and the page reads it again.
mkdir -p "$T/settingshome"
printf 'plain\n' > "$T/settings.txt"
cat > "$T/settings.script" <<SCRIPT
resize 1100 1700
wait 100
key 43 cmd ,
wait 100
dump $T/settings-page.out
key 53
wait 100
dump $T/settings-escaped.out
key 43 cmd ,
wait 100
click @settings.edit.font_size
wait 100
dump $T/settings-open.out
key 1 cmd s
wait 100
key 43 cmd ,
wait 100
click @settings.edit.ssh_auth_sock
wait 100
dump $T/settings-saved.out
quit
SCRIPT
HOME="$T/settingshome" CRC_SELFTEST="$T/settings.script" "$BIN" "$T/settings.txt" 2> "$T/settings.err"
# Three buttons and two targets (Edit, the row) per setting, all on
# screen in a window this tall.
expect "$T/settings-page.out" settings_page "open changed=0 problems=0 targets=23"
expect "$T/settings-page.out" tabs "settings.txt"
expect "$T/settings-escaped.out" settings_page "closed changed=0 problems=0 targets=23"
if ! grep -q '^tabs: .*config.toml' "$T/settings-open.out"; then
    failed "settings-open.out: $(grep -m1 '^tabs' "$T/settings-open.out"), expected a config.toml tab"
fi
expect "$T/settings-open.out" cursor "9:1"
expect_line "$T/settings-open.out" 9 "font_size = 13"
if ! grep -q '^# crc settings' "$T/settingshome/.config/crc/config.toml"; then
    failed "settings: file not created from the template"
fi
expect "$T/settings-saved.out" dirty "false"
expect "$T/settings-saved.out" message "settings applied"
# A key only commented out in the file: Edit lands on the comment.
expect "$T/settings-saved.out" cursor "36:1"

# ---- settings that cannot be used -----------------------------------------
# A line with an unknown key or a value the key does not take is said in
# the status line with its line number, at launch and again on save.
mkdir -p "$T/badsettings/.config/crc"
printf 'theme = "sepia"\ncolour = 1\nfont_size = 15\n' > "$T/badsettings/.config/crc/config.toml"
printf 'wait 300\ndump %s/badsettings.out\nquit\n' "$T" > "$T/badsettings.script"
HOME="$T/badsettings" CRC_SELFTEST="$T/badsettings.script" "$BIN" "$T/settings.txt" 2> "$T/badsettings.err"
expect "$T/badsettings.out" message 'config.toml line 1: theme takes "system", "dark" or "light", not "sepia" (and 1 more)'

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
        failed "theme-$choice.out: $(grep -m1 '^layout' "$T/theme-$choice.out"), expected theme $choice"
    fi
done
