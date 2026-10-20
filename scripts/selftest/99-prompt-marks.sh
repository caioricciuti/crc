# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh prompt-marks.

# ---- Prompt marks: Cmd-Shift-Up and Down jump between an agent's prompts ---
# A stand-in agent prints 80 lines, then for each line typed into it says
# so through the UserPromptSubmit hook, echoes it as "> <line>" and prints
# 80 lines of reply. Two prompts sent, the terminal has two marks.
# Cmd-Shift-Up from the bottom goes to the second, again to the first,
# each two lines down from the view's top; Cmd-Shift-Down goes back to
# the second, then to the output. A press on the first prompt's mark
# beside the screen jumps there too. Cmd-K clears them with the history.
# The frames are for a look at the marks in both themes.
case "$BIN" in /*) pm_bin=$BIN ;; *) pm_bin=$PWD/$BIN ;; esac
mkdir -p "$T/pmproj"
printf 'x\n' > "$T/pmproj/a.txt"
cat > "$T/pm-agent.sh" <<AGENT
#!/bin/sh
stty -echo
i=0
while [ \$i -lt 80 ]; do printf 'early %d\n' \$i; i=\$((i + 1)); done
n=0
while IFS= read -r line; do
    n=\$((n + 1))
    printf '{"prompt":"%s"}' "\$line" | "$pm_bin" --hook prompt
    printf '> %s\n' "\$line"
    i=0
    while [ \$i -lt 80 ]; do printf '%d reply %d\n' \$n \$i; i=\$((i + 1)); done
done
AGENT
chmod +x "$T/pm-agent.sh"

# pm_run <name> <theme> <script lines>: one crc run with the stand-in, in
# a home of its own.
pm_run() {
    printf '%s\n' "$3" > "$T/pm-$1.script"
    mkdir -p "$T/home/pm-$1/.config/crc"
    printf 'theme = "%s"\n' "$2" > "$T/home/pm-$1/.config/crc/config.toml"
    HOME="$T/home/pm-$1" CRC_TERMINAL_SHELL=/bin/sh CRC_CLAUDE_COMMAND="$T/pm-agent.sh" \
        CRC_SELFTEST="$T/pm-$1.script" "$BIN" "$T/pmproj" 2> "$T/pm-$1.err"
}

# Up and Down as the menu's Move Line items have them, the arrow keys'
# own characters: the menu takes the shortcut first, as it does for a
# person.
up=$(printf '\357\234\200')
down=$(printf '\357\234\201')
pm_run keys dark "wait 500
key 8 cmd,shift C
wait 700
text first prompt
key 36
wait 400
text second prompt
key 36
wait 1500
idle 300
dump $T/pm-sent.out
frame $T/pm-dark.png
key 126 cmd,shift $up
dump $T/pm-up1.out
key 126 cmd,shift $up
dump $T/pm-up2.out
frame $T/pm-dark-back.png
key 126 cmd,shift $up
dump $T/pm-up3.out
key 125 cmd,shift $down
dump $T/pm-down1.out
key 125 cmd,shift $down
dump $T/pm-down2.out
key 40 cmd k
wait 200
dump $T/pm-cleared.out
quit"
expect_terminal "$T/pm-sent.out" ' back=0 '
expect_terminal "$T/pm-sent.out" ' prompts=2 '
expect_terminal "$T/pm-up1.out" ' prompts=2 top=1 reply 78'
expect_terminal "$T/pm-up2.out" ' prompts=2 top=early 78'
# Nothing above the first: it stays.
expect_terminal "$T/pm-up3.out" ' prompts=2 top=early 78'
expect_terminal "$T/pm-down1.out" ' prompts=2 top=1 reply 78'
expect_terminal "$T/pm-down2.out" ' back=0 '
expect_terminal "$T/pm-cleared.out" ' prompts=0 '

# The first prompt's mark pressed, beside the screen: the view jumps to
# it as Cmd-Shift-Up would. In the light theme, for a look at the marks.
pm_run click light "wait 500
key 8 cmd,shift C
wait 700
text first prompt
key 36
wait 400
text second prompt
key 36
wait 1500
idle 300
frame $T/pm-light.png
click @prompt_mark.1
dump $T/pm-click.out
quit"
expect_terminal "$T/pm-click.out" ' prompts=2 top=early 78'
