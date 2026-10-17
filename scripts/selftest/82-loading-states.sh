# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh loading-states.

# ---- a project search says it is searching until it is not -----------------
# While a project search runs the bar says so, with a spinner by the words
# and rows sketched under them, and stops the moment the results land. The
# fixture is big enough that the search outlasts the dump taken as it
# starts. The frame is the loading state itself, for the eye: it goes to
# $CRC_LOADING_FRAMES when that is set, so a look at it can outlive the run.
mkdir -p "$T/slowproj"
python3 - "$T/slowproj" <<'EOF'
import os
import sys

root = sys.argv[1]
for n in range(2000):
    with open(os.path.join(root, f"note-{n:04}.txt"), "w") as f:
        for line in range(400):
            f.write(f"line {line} of note {n}: nothing to see here yet\n")
    if n % 400 == 0:
        with open(os.path.join(root, f"bed-{n:04}.txt"), "w") as f:
            f.write("plant tomatoes in bed A\n")
EOF
frames=${CRC_LOADING_FRAMES:-$T}
mkdir -p "$frames"
cat > "$T/loading.script" <<SCRIPT
resize 1440 900
wait 300
key 3 cmd,shift f
wait 200
text plant
key 36
dump $T/loading-searching.out
frame $frames/loading-project-search.bmp
wait 6000
dump $T/loading-done.out
quit
SCRIPT
CRC_SELFTEST="$T/loading.script" "$BIN" "$T/slowproj" 2> "$T/loading.err"
expect "$T/loading-searching.out" find_searching true
expect "$T/loading-done.out" find_searching false
# The palette's list is not on its way when the index is in hand.
expect "$T/loading-done.out" palette_loading false
