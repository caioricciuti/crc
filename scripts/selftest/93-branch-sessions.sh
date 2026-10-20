# Branch-specific tabs follow successful checkouts, preserving unsaved work.
mkdir -p "$T/sessionproj"
git init -q -b main "$T/sessionproj"
printf 'main notes\n' > "$T/sessionproj/notes.txt"
printf 'feature notes\n' > "$T/sessionproj/other.txt"
git -C "$T/sessionproj" add .
testgit "$T/sessionproj" commit -q -m "notes"
cat > "$T/branch-session.script" <<SCRIPT
wait 1200
key 35 cmd p
wait 200
text >switch branch
key 36
wait 300
text feature
key 36
wait 1000
key 13 cmd w
wait 300
key 35 cmd p
wait 200
text other.txt
wait 300
key 36
wait 400
dump $T/session-feature.out
key 35 cmd p
wait 200
text >switch branch
key 36
wait 300
text main
key 36
wait 1000
dump $T/session-main.out
key 35 cmd p
wait 200
text >switch branch
key 36
wait 300
text feature
key 36
wait 1000
dump $T/session-feature-again.out
key 35 cmd p
wait 200
text >switch branch
key 36
wait 300
text main
key 36
wait 1000
text unsaved
key 35 cmd p
wait 200
text >switch branch
key 36
wait 300
text feature
key 36
wait 1000
dump $T/session-dirty.out
key 18 cmd 1
dump $T/session-preserved.out
quit
SCRIPT
CRC_SELFTEST="$T/branch-session.script" "$BIN" "$T/sessionproj/notes.txt" 2> "$T/branch-session.err"
expect "$T/session-feature.out" tabs "other.txt"
expect "$T/session-main.out" tabs "notes.txt"
expect "$T/session-main.out" branch "main"
expect "$T/session-feature-again.out" tabs "other.txt"
expect "$T/session-feature-again.out" branch "feature"
expect "$T/session-dirty.out" tabs "notes.txt | other.txt"
expect "$T/session-dirty.out" active "1"
expect "$T/session-preserved.out" dirty "true"
grep -q 'unsaved' "$T/session-preserved.out" || failed "branch session lost unsaved text"
