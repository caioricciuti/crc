# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh terminal-search-all.

# ---- Find in Terminals --------------------------------------------------------
# Two workspaces on one folder (tsproj and tsproj 2), a shell each, each
# printing its own word. Option-Cmd-F lists the matches of every session
# with where each is, the first terminal's first; Return shows that
# session, in the other workspace, with its find bar open on the match.
# The words come from scripts, so the command lines typed do not match.
W="$T/tsproj"
mkdir -p "$W" "$T/home/ts-home"
printf 'x\n' > "$W/notes.txt"
echo 'echo zebralime1' > "$W/one.sh"
echo 'echo zebralime2' > "$W/two.sh"
cat > "$T/tsa.script" <<SCRIPT
wait 500
key 0 cmd,shift A
wait 900
key 45 cmd n
wait 200
text tsproj
key 36
wait 700
key 18 ctrl 1
wait 400
text sh one.sh
key 36
wait 500
key 19 ctrl 2
wait 400
text sh two.sh
key 36
wait 500
dump $T/tsa-second.out
key 3 cmd,opt f
wait 200
text zebralime
wait 400
wait 400
dump $T/tsa-open.out
key 36
wait 500
dump $T/tsa-picked.out
quit
SCRIPT
HOME="$T/home/ts-home" CRC_TERMINAL_SHELL=/bin/sh \
    CRC_SELFTEST="$T/tsa.script" "$BIN" "$W" 2> "$T/tsa.err"
expect "$T/tsa-second.out" agents "on tasks=tsproj,tsproj 2 selected=tsproj 2 sessions=tsproj,*tsproj 2 claude=0 pair=- op=-"
expect "$T/tsa-open.out" palette_query "zebralime"
expect "$T/tsa-open.out" palette_rows "zebralime1 (tsproj / sh  ·  line 2)|zebralime2 (tsproj 2 / sh  ·  line 2)"
expect "$T/tsa-picked.out" palette_query ""
expect "$T/tsa-picked.out" agents "on tasks=tsproj,tsproj 2 selected=tsproj sessions=*tsproj,tsproj 2 claude=0 pair=- op=-"
expect_terminal "$T/tsa-picked.out" 'find=zebralime 1 of 1 target=terminal keys=true'
