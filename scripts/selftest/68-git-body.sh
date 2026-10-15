# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh git-body.

# ---- a commit message with a body --------------------------------------------
# Return and Shift-Return break the line in the message field, which grows
# a line at a time; Up and a click move the caret between lines; Cmd-Return
# commits the whole message. A body line past 72 characters is tinted from
# the guide on. Return in a document beside the panel still types a line.
mkdir -p "$T/bodyproj" "$T/bodyhome"
git -C "$T/bodyproj" init -q
git -C "$T/bodyproj" config user.name 'caio GUI test'
git -C "$T/bodyproj" config user.email 'test@example.invalid'
git -C "$T/bodyproj" config commit.gpgsign false
printf 'tomatoes\n' > "$T/bodyproj/garden.txt"
git -C "$T/bodyproj" add garden.txt
git -C "$T/bodyproj" commit -q -m 'start the garden'
printf 'basil\n' > "$T/bodyproj/herbs.txt"
git -C "$T/bodyproj" add herbs.txt
long='This body line keeps going past the seventy-two column guide, so its tail turns amber.'
cat > "$T/body.script" <<SCRIPT
wait 400
key 5 cmd,opt g
wait 600
idle 1000
frame $T/body-empty.bmp
click @git.message
text feat: grow the field
wait 100
dump $T/body-subject.out
frame $T/body-subject.bmp
text  as the body grows past one line
wait 100
idle 300
dump $T/body-long-subject.out
frame $T/body-long-subject.bmp
key 36
key 36 shift
wait 100
dump $T/body-blank.out
text First body line.
key 36
text Second body line.
key 36
text $long
wait 100
idle 300
dump $T/body-full.out
frame $T/body-full.bmp
key 126
wait 100
idle 300
dump $T/body-up.out
clickin @git.message 10 50
wait 100
dump $T/body-click.out
key 36 cmd
wait 700
idle 1000
dump $T/body-done.out
quit
SCRIPT
HOME="$T/bodyhome" CRC_SELFTEST="$T/body.script" "$BIN" "$T/bodyproj" 2> "$T/body.err"
subject='feat: grow the field as the body grows past one line'
expect "$T/body-subject.out" git_message 'feat: grow the field'
expect "$T/body-subject.out" git_message_lines 1
expect "$T/body-long-subject.out" git_subject_len 52
expect "$T/body-long-subject.out" git_message_caret '1:53'
expect "$T/body-blank.out" git_message "$subject\\n\\n"
expect "$T/body-blank.out" git_message_lines 3
expect "$T/body-full.out" git_focus true
expect "$T/body-full.out" git_message "$subject\\n\\nFirst body line.\\nSecond body line.\\n$long"
expect "$T/body-full.out" git_message_lines 5
expect "$T/body-full.out" git_subject_len 52
expect "$T/body-full.out" git_message_caret '5:87'
# Up keeps the column it can: the end of the shorter line above.
expect "$T/body-up.out" git_message_caret '4:18'
# The third line's row, near its start: 5pt inset, then 18pt a line.
expect "$T/body-click.out" git_message_caret '3:1'
expect "$T/body-done.out" git_message ''
expect "$T/body-done.out" git_changes 0
want=$(printf '%s\n\nFirst body line.\nSecond body line.\n%s' "$subject" "$long")
got=$(git -C "$T/bodyproj" log -1 --format=%B)
[ "$got" = "$want" ] || failed "git-body: HEAD's message is [$got]"

# Return in a document with Source Control open is the document's.
cat > "$T/body-doc.script" <<SCRIPT
wait 400
key 5 cmd,opt g
wait 600
text ab
key 36
text cd
wait 100
dump $T/body-doc.out
quit
SCRIPT
HOME="$T/bodyhome" CRC_SELFTEST="$T/body-doc.script" "$BIN" "$T/bodyproj/garden.txt" 2> "$T/body-doc.err"
expect "$T/body-doc.out" git_open true
expect "$T/body-doc.out" git_focus false
expect "$T/body-doc.out" git_message ''
expect_line "$T/body-doc.out" 1 'ab'
expect_line "$T/body-doc.out" 2 'cdtomatoes'
[ -z "${SELFTEST_FRAMES:-}" ] || cp -f "$T/body-empty.bmp" "$T/body-subject.bmp" "$T/body-long-subject.bmp" "$T/body-full.bmp" "$SELFTEST_FRAMES/"
