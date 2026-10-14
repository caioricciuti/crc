# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh blame-heat.

# ---- blame as heat in the gutter ----------------------------------------------
# garden.txt was written over three commits: two years ago, two months ago
# and two hours ago. Git > Blame Heat is off until asked for; on, the strip
# shades the lines of the last two (the two-year-old line is past fading)
# in two runs, each with its commit as a tooltip. An edit hides the strip
# from the edited line down at once, and a second after typing stops the
# file is blamed again, the edited line now not committed. Off drops it.
# Every commit here is unsigned, so every run is dashed and says so.
# A Command click on the strip opens File History at the commit that last
# changed the line, the caret on it; so does a plain click where the
# strip is drawn, but not beside the two-year-old line, which has none.
# A second launch with blame_heat = true in the settings has it on from
# the start, drawn in the light theme, next to a change bar.
mkdir -p "$T/heatproj"
git init -q -b main "$T/heatproj"
heatgit() {
    local ago=$1
    shift
    local when
    when="@$(($(date +%s) - ago)) +0000"
    GIT_AUTHOR_DATE="$when" GIT_COMMITTER_DATE="$when" testgit "$T/heatproj" "$@"
}
printf 'beans\npeas\nleeks\n' > "$T/heatproj/garden.txt"
git -C "$T/heatproj" add garden.txt
heatgit $((2 * 365 * 86400)) commit -q -m "plant the beds"
printf 'Beans\nPeas\nleeks\n' > "$T/heatproj/garden.txt"
heatgit $((60 * 86400)) commit -q -am "name the rows"
printf 'Beans\nPeas\nleeks\nkale\nchard\n' > "$T/heatproj/garden.txt"
heatgit 7200 commit -q -am "add greens"
cat > "$T/heat.script" <<SCRIPT
wait 800
wait 600
wait 300
dump $T/heat-off.out
key 35 cmd p
wait 200
text >blame heat
dump $T/heat-palette.out
key 36
wait 600
wait 300
dump $T/heat-on.out
frame $T/heat-dark.bmp
clickin @heat.3 1 4 1 cmd
wait 600
wait 300
dump $T/heat-cmd-click.out
key 53
wait 200
click @heat.4
wait 600
wait 300
dump $T/heat-click.out
key 53
wait 200
click @heat.3
wait 300
dump $T/heat-old-click.out
key 126 cmd
key 125
key 125
key 125
text x
wait 50
dump $T/heat-edited.out
wait 1500
wait 300
dump $T/heat-again.out
frame $T/heat-mixed.bmp
key 35 cmd p
wait 200
text >blame heat
key 36
wait 300
dump $T/heat-toggled-off.out
quit
SCRIPT
CRC_SELFTEST="$T/heat.script" "$BIN" "$T/heatproj/garden.txt" 2> "$T/heat.err"
expect "$T/heat-off.out" blame_heat "off"
expect "$T/heat-palette.out" palette_first "Blame Heat"
grep -Eq '^blame_heat: on lines=5 commits=3 unsigned=3 shown=4 hidden_from=- spots=2 tip=[0-9a-f]{7}  Tester  2 months ago / name the rows / unsigned / Cmd-click: open in File History$' "$T/heat-on.out" \
    || failed "heat-on.out: $(grep '^blame_heat:' "$T/heat-on.out")"
heat_hash() { git -C "$T/heatproj" rev-parse --short=7 "$1"; }
# Command click beside leeks: the first commit, the caret on line 3.
case "$(grep -m1 '^scrub: ' "$T/heat-cmd-click.out")" in
    "scrub: 1/3@$(heat_hash HEAD~2) "*" first=beans") ;;
    *) failed "heat: cmd-click: $(grep -m1 '^scrub: ' "$T/heat-cmd-click.out")" ;;
esac
expect "$T/heat-cmd-click.out" cursor "3:1"
case "$(grep -m1 '^message: ' "$T/heat-cmd-click.out")" in
    "message: $(heat_hash HEAD~2) Tester, "*" ago: plant the beds (1 of 3)") ;;
    *) failed "heat: cmd-click: $(grep -m1 '^message: ' "$T/heat-cmd-click.out")" ;;
esac
# A plain click beside kale: the last commit.
case "$(grep -m1 '^scrub: ' "$T/heat-click.out")" in
    "scrub: 3/3@$(heat_hash HEAD) "*" first=Beans") ;;
    *) failed "heat: click: $(grep -m1 '^scrub: ' "$T/heat-click.out")" ;;
esac
expect "$T/heat-click.out" cursor "4:1"
expect "$T/heat-old-click.out" scrub "closed"
expect_line "$T/heat-edited.out" 4 "xkale"
grep -Eq '^blame_heat: on lines=5 commits=3 unsigned=3 shown=2 hidden_from=4 spots=1 ' "$T/heat-edited.out" \
    || failed "heat-edited.out: $(grep '^blame_heat:' "$T/heat-edited.out")"
grep -Eq '^blame_heat: on lines=5 commits=3 unsigned=3 shown=3 hidden_from=- spots=2 ' "$T/heat-again.out" \
    || failed "heat-again.out: $(grep '^blame_heat:' "$T/heat-again.out")"
expect "$T/heat-toggled-off.out" blame_heat "off"
# Nothing was written: the edit stays in the buffer.
[ "$(cat "$T/heatproj/garden.txt")" = "$(printf 'Beans\nPeas\nleeks\nkale\nchard')" ] \
    || failed "heat: garden.txt was written: $(cat "$T/heatproj/garden.txt")"

mkdir -p "$T/heathome/.config/crc"
printf 'theme = "light"\nblame_heat = true\n' > "$T/heathome/.config/crc/config.toml"
cat > "$T/heat-light.script" <<SCRIPT
wait 800
wait 600
wait 300
dump $T/heat-light.out
frame $T/heat-light.bmp
key 125
key 125
text x
wait 1500
wait 300
dump $T/heat-light-again.out
frame $T/heat-light-mixed.bmp
quit
SCRIPT
HOME="$T/heathome" CRC_SELFTEST="$T/heat-light.script" "$BIN" "$T/heatproj/garden.txt" 2> "$T/heat-light.err"
grep -Eq '^blame_heat: on lines=5 commits=3 unsigned=3 shown=4 hidden_from=- spots=2 ' "$T/heat-light.out" \
    || failed "heat-light.out: $(grep '^blame_heat:' "$T/heat-light.out")"
grep -q '^layout: .* theme light$' "$T/heat-light.out" \
    || failed "heat-light.out: $(grep '^layout:' "$T/heat-light.out")"
# leeks edited: a change bar between the two runs of the strip.
expect "$T/heat-light-again.out" gutter "3M"
grep -Eq '^blame_heat: on lines=5 commits=2 unsigned=2 shown=4 hidden_from=- spots=2 ' "$T/heat-light-again.out" \
    || failed "heat-light-again.out: $(grep '^blame_heat:' "$T/heat-light-again.out")"
[ -z "${SELFTEST_FRAMES:-}" ] || cp -f "$T/heat-dark.bmp" "$T/heat-mixed.bmp" "$T/heat-light.bmp" "$T/heat-light-mixed.bmp" "$SELFTEST_FRAMES/"
