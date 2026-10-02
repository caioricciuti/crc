# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh workspace.

# ---- a workspace of several repositories --------------------------------------
# A folder holding notes and two repositories. Source Control shows the
# first, names it in a menu at the top, and the menu (or Git > Switch
# Repository) switches to the other. A plain repository has no menu.
mkdir -p "$T/ws/docs"
printf 'what is next\n' > "$T/ws/docs/state.md"
for repo in app site; do
    git init -q -b main "$T/ws/$repo"
    printf 'one\n' > "$T/ws/$repo/readme.txt"
    git -C "$T/ws/$repo" add readme.txt
    testgit "$T/ws/$repo" commit -q -m "first"
done
printf 'two\n' >> "$T/ws/app/readme.txt"
printf 'new\n' > "$T/ws/site/a.txt"
printf 'new\n' > "$T/ws/site/b.txt"
cat > "$T/ws.script" <<SCRIPT
wait 800
wait 600
key 35 cmd p
wait 200
text >source control
key 36
wait 600
wait 300
dump $T/ws-first.out
click @git.repo
wait 300
dump $T/ws-picker.out
text site
key 36
wait 600
wait 300
dump $T/ws-site.out
key 35 cmd p
wait 200
text >switch repository
key 36
wait 300
dump $T/ws-menu.out
key 53
quit
SCRIPT
CRC_SELFTEST="$T/ws.script" "$BIN" "$T/ws" 2> "$T/ws.err"
expect "$T/ws-first.out" repo "app"
expect "$T/ws-first.out" git_changes 1
expect "$T/ws-first.out" branch "main"
expect "$T/ws-picker.out" palette_first "app"
expect "$T/ws-site.out" repo "site"
expect "$T/ws-site.out" git_changes 2
expect "$T/ws-menu.out" palette_first "app"

cat > "$T/ws-plain.script" <<SCRIPT
wait 800
wait 600
key 35 cmd p
wait 200
text >source control
key 36
wait 600
wait 300
dump $T/ws-plain.out
quit
SCRIPT
CRC_SELFTEST="$T/ws-plain.script" "$BIN" "$T/ws/app" 2> "$T/ws-plain.err"
expect "$T/ws-plain.out" repo ""
expect "$T/ws-plain.out" git_changes 1

# ---- Home for a workspace ----------------------------------------------------
# The state doc's "waiting" list and each repository are on Home; the
# state doc, written after the launches above, is a note changed since
# the last visit. A second visit lists the commits made since the first.
mkdir -p "$T/ws/.crc"
printf 'state = "docs/state.md"\nwaiting_heading = "for me"\n' > "$T/ws/.crc/workspace.toml"
printf '# State\n\n## Open, for me\n\n- Water the tomatoes\n- Read the seed catalogue\n' > "$T/ws/docs/state.md"
cat > "$T/ws-home.script" <<SCRIPT
wait 800
wait 600
wait 300
dump $T/ws-home.out
quit
SCRIPT
CRC_SELFTEST="$T/ws-home.script" "$BIN" "$T/ws" 2> "$T/ws-home.err"
expect "$T/ws-home.out" home "waiting=Water the tomatoes|Read the seed catalogue repos=app:main:1,site:main:2 commits=0 notes=1 hits=10"
# The visit is recorded with a one-second clock: wait a second so the
# commit is after it.
sleep 1.1
printf 'three\n' > "$T/ws/site/c.txt"
git -C "$T/ws/site" add c.txt
testgit "$T/ws/site" commit -q -m "plant the beans"
CRC_SELFTEST="$T/ws-home.script" "$BIN" "$T/ws" 2> "$T/ws-home2.err"
grep -q '^home: .* commits=1 ' "$T/ws-home.out" \
    || failed "ws-home.out: the second visit does not list the new commit: $(grep '^home:' "$T/ws-home.out")"
