# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh local-history.

# ---- local history: what is kept, and when ------------------------------------
# notes.txt: its opened text before the first change, each save, and the
# unsaved text once typing stops (400 ms here instead of 30 seconds). The
# save after that, of the same text, adds nothing. other.txt: changed on
# disk while clean, the text the reload replaced; then the reloaded text
# before its first change, and the edited text when its tab is closed
# with Don't Save. A .env file is a secret: edited and saved, nothing.
# Rows are reason:size, oldest first (1 opened, 2 saved, 3 idle,
# 4 reloaded, 5 closed unsaved). Nothing goes to Application Support.
mkdir -p "$T/histproj"
printf 'first\n' > "$T/histproj/notes.txt"
printf 'other\n' > "$T/histproj/other.txt"
printf 'TOKEN=1\n' > "$T/histproj/.env"
histdb="$T/hist/history.db"
cat > "$T/hist.script" <<SCRIPT
wait 800
key 35 cmd p
text notes.txt
key 36
wait 300
text a
key 1 cmd s
wait 200
text b
wait 600
wait 300
key 1 cmd s
wait 200
key 35 cmd p
text .env
key 36
wait 300
text X
key 1 cmd s
wait 600
wait 300
key 35 cmd p
text other.txt
key 36
wait 300
write $T/histproj/other.txt changed
wait 500
wait 300
dump $T/hist-reloaded.out
text x
key 13 cmd w
wait 300
dump $T/hist-closed.out
quit
SCRIPT
mkdir -p "$T/hist"
CRC_HISTORY_DB="$histdb" CRC_HISTORY_IDLE_MS=400 CRC_DISCARD_ANSWER=dont-save \
    CRC_SELFTEST="$T/hist.script" "$BIN" "$T/histproj" 2> "$T/hist.err"
expect_line "$T/hist-reloaded.out" 1 "changed"
expect "$T/hist-reloaded.out" dirty "false"
case "$(grep -m1 '^tabs: ' "$T/hist-closed.out")" in
    *other.txt*) failed "hist: other.txt was not closed: $(grep -m1 '^tabs: ' "$T/hist-closed.out")" ;;
esac
[ "$(cat "$T/histproj/other.txt")" = "changed" ] \
    || failed "hist: other.txt was written: $(cat "$T/histproj/other.txt")"
# hist_rows <file name>: reason:size of each kept version, oldest first.
hist_rows() {
    /usr/bin/sqlite3 "$histdb" "SELECT group_concat(row, ' ') FROM (
        SELECT s.reason || ':' || b.size AS row FROM snapshots s
        JOIN files f ON f.id = s.file JOIN blobs b ON b.id = s.blob
        WHERE f.path LIKE '%/histproj/$1' ORDER BY s.id)" 2>&1
}
[ "$(hist_rows notes.txt)" = "1:6 2:7 3:8" ] \
    || failed "hist: notes.txt rows are [$(hist_rows notes.txt)], expected [1:6 2:7 3:8]"
[ "$(hist_rows other.txt)" = "4:6 1:8 5:9" ] \
    || failed "hist: other.txt rows are [$(hist_rows other.txt)], expected [4:6 1:8 5:9]"
[ -z "$(hist_rows .env)" ] \
    || failed "hist: .env was kept: [$(hist_rows .env)]"
[ "$(stat -f %Lp "$histdb")" = "600" ] \
    || failed "hist: history.db is mode $(stat -f %Lp "$histdb"), expected 600"
[ ! -e "$HOME/Library/Application Support/crc/history.db" ] \
    || failed "hist: a history.db was written in Application Support"
