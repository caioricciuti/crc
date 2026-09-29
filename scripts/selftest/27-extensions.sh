# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh extensions.

# ---- extensions ---------------------------------------------------------------
# A registry on disk, signed with a throwaway key the test instance is told
# to trust (only honoured under CRC_SELFTEST), serving the Sort Lines module
# built in crc-extensions. Cmd-Shift-X opens the page; Install asks first
# and lists what it may do; the commands land in the palette; Sort Lines
# sorts the selection, removes duplicates from the whole document with
# nothing selected, turns off, reinstalls unsigned from a folder, and
# uninstalls.
X="$PWD/tests/fixtures/extensions/sort-lines"
mkdir -p "$T/extreg" "$T/exthome" "$T/extproj" "$T/extfolder"
cp "$X"/* "$T/extfolder/"
cp "$X/sort_lines.wasm" "$T/extreg/crc.sort-lines-0.1.0.wasm"
python3 - "$X" "$T/extreg" <<'PY'
import hashlib, json, pathlib, sys
src, out = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
entry = json.loads((src / "manifest.json").read_text())
wasm = (src / "sort_lines.wasm").read_bytes()
entry.update(wasm="crc.sort-lines-0.1.0.wasm", sha256=hashlib.sha256(wasm).hexdigest(),
             size=len(wasm), readme=(src / "README.md").read_text())
(out / "index.json").write_text(json.dumps({"api": 1, "extensions": [entry]}))
PY
/usr/bin/openssl ecparam -name prime256v1 -genkey -noout -out "$T/extreg/key.pem" 2>/dev/null
/usr/bin/openssl ec -in "$T/extreg/key.pem" -pubout -out "$T/extreg/pub.pem" 2>/dev/null
/usr/bin/openssl dgst -sha256 -sign "$T/extreg/key.pem" -out "$T/extreg/index.json.sig" "$T/extreg/index.json"
printf 'pear\napple\nfig\napple\n' > "$T/extproj/notes.txt"
cat > "$T/ext.script" <<SCRIPT
wait 600
key 7 cmd,shift x
wait 600
wait 300
dump $T/ext-open.out
click @extensions.select.crc.sort-lines
wait 200
dump $T/ext-details.out
click @extensions.install.crc.sort-lines
wait 200
dump $T/ext-confirm.out
click @extensions.confirm
wait 600
wait 300
dump $T/ext-installed.out
key 53
key 0 cmd a
key 35 cmd p
wait 200
text >sort lines
wait 200
dump $T/ext-palette.out
key 36
wait 600
wait 300
dump $T/ext-sorted.out
key 126 cmd
key 35 cmd p
wait 200
text >remove duplicate
key 36
wait 600
wait 300
dump $T/ext-whole.out
key 7 cmd,shift x
wait 600
wait 300
click @extensions.select.crc.sort-lines
wait 200
click @extensions.toggle.crc.sort-lines
wait 200
dump $T/ext-off.out
click @extensions.home
wait 200
dump $T/ext-home.out
click @extensions.folder
wait 200
dump $T/ext-folder.out
click @extensions.confirm
wait 300
dump $T/ext-unsigned.out
click @extensions.uninstall.crc.sort-lines
wait 300
dump $T/ext-removed.out
quit
SCRIPT
HOME="$T/exthome" CRC_EXT_REGISTRY="file://$T/extreg/" CRC_EXT_REGISTRY_KEY="$T/extreg/pub.pem" CRC_EXT_FOLDER="$T/extfolder" CRC_SELFTEST="$T/ext.script" "$BIN" "$T/extproj/notes.txt" 2> "$T/ext.err"
# The page opens on its home: the header with Close, Refresh and Install
# from Folder, and the row in the sidebar, each a pointing hand.
expect "$T/ext-open.out" extensions "open selected=- installed= registry=ready:crc.sort-lines confirm=- busy=- note=-"
expect "$T/ext-open.out" pointer_targets 4
# One extension: the way back, Install, and the row; no header.
expect "$T/ext-details.out" extensions "open selected=crc.sort-lines installed= registry=ready:crc.sort-lines confirm=- busy=- note=-"
expect "$T/ext-details.out" pointer_targets 3
expect "$T/ext-confirm.out" extensions "open selected=crc.sort-lines installed= registry=ready:crc.sort-lines confirm=crc.sort-lines busy=- note=-"
expect "$T/ext-installed.out" extensions "open selected=crc.sort-lines installed=crc.sort-lines:0.1.0:on:signed registry=ready:crc.sort-lines confirm=- busy=- note=Installed Sort Lines 0.1.0"
expect "$T/ext-installed.out" ext_commands "Sort Lines|Sort Lines Descending|Remove Duplicate Lines"
expect "$T/ext-palette.out" palette_first "Sort Lines"
# Escape gives the column back; the list stays in the sidebar.
expect "$T/ext-sorted.out" extensions "list selected=crc.sort-lines installed=crc.sort-lines:0.1.0:on:signed registry=ready:crc.sort-lines confirm=- busy=- note=Installed Sort Lines 0.1.0"
expect_line "$T/ext-sorted.out" 1 "apple"
expect_line "$T/ext-sorted.out" 2 "apple"
expect_line "$T/ext-sorted.out" 3 "fig"
expect_line "$T/ext-sorted.out" 4 "pear"
expect "$T/ext-sorted.out" dirty true
expect_line "$T/ext-whole.out" 1 "apple"
expect_line "$T/ext-whole.out" 2 "fig"
expect_line "$T/ext-whole.out" 3 "pear"
expect "$T/ext-whole.out" message "removed 1 duplicate line"
expect "$T/ext-off.out" ext_commands ""
expect "$T/ext-home.out" extensions "open selected=- installed=crc.sort-lines:0.1.0:off:signed registry=ready:crc.sort-lines confirm=- busy=- note=Sort Lines is off"
expect "$T/ext-folder.out" extensions "open selected=crc.sort-lines installed=crc.sort-lines:0.1.0:off:signed registry=ready:crc.sort-lines confirm=crc.sort-lines busy=- note=Sort Lines is off"
expect "$T/ext-unsigned.out" extensions "open selected=crc.sort-lines installed=crc.sort-lines:0.1.0:off:unsigned registry=ready:crc.sort-lines confirm=- busy=- note=Installed Sort Lines 0.1.0, unsigned"
expect "$T/ext-removed.out" extensions "open selected=crc.sort-lines installed= registry=ready:crc.sort-lines confirm=- busy=- note=Removed Sort Lines"
[ -z "$(ls -A "$T/exthome/Library/Application Support/crc/extensions" 2>/dev/null)" ] \
    || { echo "FAIL ext: files left after uninstall: $(ls -A "$T/exthome/Library/Application Support/crc/extensions")"; fail=1; }
