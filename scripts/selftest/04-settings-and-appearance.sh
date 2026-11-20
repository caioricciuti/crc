# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh settings-and-appearance.

# ---- settings -------------------------------------------------------------
# Cmd-, opens the Settings page in the editor column; Escape gives the
# column back. A switch, a stepper or a choice rewrites its one line of
# config.toml, created from the template when there is none, and applies
# it at once. A setting that takes text opens the file with the caret on
# that key's line. Saving the tab applies it and the page reads it again.
mkdir -p "$T/settingshome"
printf 'plain\n' > "$T/settings.txt"
cat > "$T/settings.script" <<SCRIPT
resize 1100 1000
wait 100
key 43 cmd ,
wait 100
dump $T/settings-page.out
key 53
wait 100
dump $T/settings-escaped.out
key 43 cmd ,
wait 100
click @settings.edit.ssh_auth_sock
wait 100
dump $T/settings-open.out
key 1 cmd s
wait 100
dump $T/settings-saved.out
key 43 cmd ,
wait 100
click @settings.toggle.caret_blink
wait 100
click @settings.more.font_size
wait 100
click @settings.choose.theme.light
wait 300
dump $T/settings-controls.out
click @settings.edit.font
wait 100
dump $T/settings-font.out
quit
SCRIPT
HOME="$T/settingshome" CRC_SELFTEST="$T/settings.script" "$BIN" "$T/settings.txt" 2> "$T/settings.err"
# Close, Reload, Open config.toml, and a target per control on screen
# (a choice has one per word, a stepper one per step, a text row one).
# The window is 1000 pt tall, so it is that tall on any display (a taller
# one was cut to the screen, and the count followed whichever display the
# window opened on): the last settings are below the fold.
expect "$T/settings-page.out" settings_page "open changed=0 problems=0 targets=19"
expect "$T/settings-page.out" tabs "settings.txt"
expect "$T/settings-escaped.out" settings_page "closed changed=0 problems=0 targets=19"
if ! grep -q '^tabs: .*config.toml' "$T/settings-open.out"; then
    failed "settings-open.out: $(grep -m1 '^tabs' "$T/settings-open.out"), expected a config.toml tab"
fi
if ! grep -q '^# crc settings' "$T/settingshome/.config/crc/config.toml"; then
    failed "settings: file not created from the template"
fi
# A key only commented out in the file: Edit lands on the comment.
expect "$T/settings-open.out" cursor "36:1"
expect "$T/settings-saved.out" dirty "false"
expect "$T/settings-saved.out" message "settings applied"
# The controls: each wrote its own line, the comments stayed, and each
# applied at once.
settings_file="$T/settingshome/.config/crc/config.toml"
for line in '^# crc settings' '^# false keeps the caret solid\.$' '^caret_blink = false$' '^font_size = 14$' '^theme = "light"$'; do
    grep -q "$line" "$settings_file" || failed "settings: config.toml has no line matching $line"
done
grep -q "^settings_page: open changed=3 " "$T/settings-controls.out" \
    || failed "settings-controls.out: $(grep '^settings_page:' "$T/settings-controls.out"), expected three changed"
grep -q '^layout: .* theme light$' "$T/settings-controls.out" \
    || failed "settings-controls.out: $(grep -m1 '^layout' "$T/settings-controls.out"), expected theme light"
# A text setting opens the file at its line; the tab, open and clean while
# the controls wrote the file, shows what they wrote.
expect "$T/settings-font.out" cursor "6:1"
expect_line "$T/settings-font.out" 6 'font = "SF Mono"'
expect_line "$T/settings-font.out" 15 'caret_blink = false'

# ---- a control with config.toml open and unsaved --------------------------
# The page does not write under unsaved edits: the file and the tab stay as
# they were, and the status line says what to do.
mkdir -p "$T/guardhome"
cat > "$T/guard.script" <<SCRIPT
resize 1100 1000
wait 100
key 43 cmd ,
wait 100
click @settings.edit.font
wait 100
text x
wait 100
key 43 cmd ,
wait 100
click @settings.toggle.caret_blink
wait 100
dump $T/guard.out
quit
SCRIPT
HOME="$T/guardhome" CRC_SELFTEST="$T/guard.script" "$BIN" "$T/settings.txt" 2> "$T/guard.err"
expect "$T/guard.out" message "Save or revert config.toml first"
expect "$T/guard.out" dirty "true"
expect_line "$T/guard.out" 6 'xfont = "SF Mono"'
grep -q "^settings_page: open changed=0 " "$T/guard.out" \
    || failed "guard.out: $(grep '^settings_page:' "$T/guard.out"), expected nothing changed"
guard_file="$T/guardhome/.config/crc/config.toml"
grep -q '^caret_blink = true$' "$guard_file" || failed "guard: config.toml was written under unsaved edits"
grep -q '^font = "SF Mono"$' "$guard_file" || failed "guard: config.toml lost its font line"

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
