# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh lsp-code-actions.

# ---- language server: code actions ------------------------------------------
# The fake server offers, on a TODO line, a preferred fix it holds back until
# resolved, a command that comes back as workspace/applyEdit, a disabled
# action, and Organize Imports. Resting the caret shows the bulb; Cmd-. lists
# the actions, preferred first and disabled last; Shift-Option-O organizes.
mkdir -p "$T/lsp3proj"
printf 'import b\nimport a\nx = 1  # TODO\n' > "$T/lsp3proj/main.py"
cat > "$T/lsp3.script" <<SCRIPT
wait 800
wait 400
wait 400
key 125 -
key 125 -
wait 600
wait 400
dump $T/lsp3-bulb.out
key 47 cmd .
wait 300
dump $T/lsp3-list.out
key 36
wait 400
wait 300
dump $T/lsp3-fixed.out
key 47 cmd .
wait 600
wait 300
text upper
key 36
wait 400
wait 300
dump $T/lsp3-command.out
key 31 shift,opt o
wait 400
wait 300
dump $T/lsp3-organized.out
quit
SCRIPT
CRC_LSP_FAKE="$PWD/scripts/fake-lsp.py" CRC_SELFTEST="$T/lsp3.script" "$BIN" "$T/lsp3proj/main.py" 2> "$T/lsp3.err"
expect "$T/lsp3-bulb.out" bulb "3 at 18"
expect "$T/lsp3-bulb.out" pointer_targets 1
expect "$T/lsp3-list.out" actions "Replace TODO with DONE|Organize Imports|Upper-case this line|Extract function"
expect_line "$T/lsp3-fixed.out" 3 "x = 1  # DONE"
expect "$T/lsp3-fixed.out" message "Replace TODO with DONE: changed 1 file, 1 open and unsaved"
expect_line "$T/lsp3-command.out" 3 "X = 1  # DONE"
expect "$T/lsp3-command.out" message "changed 1 file, 1 open and unsaved"
expect_line "$T/lsp3-organized.out" 1 "import a"
expect_line "$T/lsp3-organized.out" 2 "import b"
expect "$T/lsp3-organized.out" message "Organize Imports: changed 1 file, 1 open and unsaved"

# Organize, then format, on save: both settings on, and the file on disk
# ends up sorted and without trailing spaces.
mkdir -p "$T/lsp4home/.config/crc" "$T/lsp4proj"
printf 'organize_imports_on_save = true\nformat_on_save = true\n' > "$T/lsp4home/.config/crc/config.toml"
printf 'import b\nimport a   \nx = 1\n' > "$T/lsp4proj/main.py"
cat > "$T/lsp4.script" <<SCRIPT
wait 800
wait 400
wait 400
key 125 cmd
text y = 2
key 1 cmd s
wait 400
wait 400
wait 300
dump $T/lsp4-saved.out
quit
SCRIPT
HOME="$T/lsp4home" CRC_LSP_FAKE="$PWD/scripts/fake-lsp.py" CRC_SELFTEST="$T/lsp4.script" "$BIN" "$T/lsp4proj/main.py" 2> "$T/lsp4.err"
[ "$(cat "$T/lsp4proj/main.py")" = "$(printf 'import a\nimport b\nx = 1\ny = 2')" ] \
    || { echo "FAIL lsp4: main.py is: $(cat "$T/lsp4proj/main.py")"; fail=1; }
expect "$T/lsp4-saved.out" dirty false
