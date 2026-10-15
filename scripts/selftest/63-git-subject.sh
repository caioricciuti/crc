# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh git-subject.

# ---- the commit subject's length ---------------------------------------------
# The message field counts the subject's characters once there is a
# message, right-aligned in the field, and the text clips short of the
# count. A short subject, then typed on to 60 characters, each dumped and drawn.
mkdir -p "$T/subjectproj"
git init -q -b main "$T/subjectproj"
printf 'one\n' > "$T/subjectproj/notes.txt"
git -C "$T/subjectproj" add notes.txt
testgit "$T/subjectproj" commit -q -m notes
printf 'two\n' >> "$T/subjectproj/notes.txt"
cat > "$T/subject.script" <<SCRIPT
wait 800
wait 600
key 35 cmd p
wait 200
text >source control
key 36
wait 600
wait 300
click @git.message
wait 100
dump $T/subject-empty.out
text feat: count the subject
wait 100
dump $T/subject-short.out
frame $T/subject-short.bmp
text  and colour it as it runs past fifty.
wait 100
dump $T/subject-long.out
frame $T/subject-long.bmp
quit
SCRIPT
CRC_SELFTEST="$T/subject.script" "$BIN" "$T/subjectproj" 2> "$T/subject.err"
expect "$T/subject-empty.out" git_focus true
expect "$T/subject-empty.out" git_subject_len 0
expect "$T/subject-short.out" git_message "feat: count the subject"
expect "$T/subject-short.out" git_subject_len 23
expect "$T/subject-long.out" git_message "feat: count the subject and colour it as it runs past fifty."
expect "$T/subject-long.out" git_subject_len 60
[ -z "${SELFTEST_FRAMES:-}" ] || cp -f "$T/subject-short.bmp" "$T/subject-long.bmp" "$SELFTEST_FRAMES/"
