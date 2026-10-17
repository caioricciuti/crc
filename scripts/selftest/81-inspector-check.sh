# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh inspector-check.

# ---- The inspector's Check: a fetch, so Remote says what the remote has ---
# A folder of two repositories, orchard and vineyard, each a clone of its
# own bare origin. Someone else pushes to orchard's origin. The inspector
# compares HEAD with the refs as last fetched, so orchard reads up to date
# and not checked; Pull is live anyway (it fetches itself). Check on
# orchard's card fetches: orchard then has one to pull and says it was
# checked, and vineyard is not checked. Pull brings the pushed file.
# Someone else pushed to vineyard's origin too: the header's refresh
# checks both, so both say checked, and only vineyard has one to pull.
CK="$T/check-many"
mkdir -p "$CK"
for r in orchard vineyard; do
    git init -q --bare -b main "$T/check-$r.git"
    git clone -q "$T/check-$r.git" "$CK/$r" 2>/dev/null
    printf 'apple\n' > "$CK/$r/trees.txt"
    testgit "$CK/$r" config user.name Tester
    testgit "$CK/$r" config user.email t@example.com
    testgit "$CK/$r" config commit.gpgsign false
    testgit "$CK/$r" add trees.txt
    testgit "$CK/$r" commit -qm 'plant apple'
    testgit "$CK/$r" push -q -u origin main
done
git clone -q "$T/check-orchard.git" "$T/check-other"
printf 'pear\n' > "$T/check-other/pear.txt"
testgit "$T/check-other" add pear.txt
testgit "$T/check-other" commit -qm 'plant pear'
testgit "$T/check-other" push -q origin main
git clone -q "$T/check-vineyard.git" "$T/check-other-vineyard"
printf 'grape\n' > "$T/check-other-vineyard/grape.txt"
testgit "$T/check-other-vineyard" add grape.txt
testgit "$T/check-other-vineyard" commit -qm 'plant grape'
testgit "$T/check-other-vineyard" push -q origin main
cat > "$T/inspector-check.script" <<SCRIPT
wait 500
key 0 cmd,shift A
wait 700
wait 700
wait 500
dump $T/inspector-check-before.out
click @inspector.check
wait 700
wait 700
wait 500
dump $T/inspector-check-after.out
click @inspector.pull
wait 700
wait 700
wait 500
dump $T/inspector-check-pulled.out
click @inspector.refresh
wait 700
wait 700
wait 500
dump $T/inspector-check-all.out
quit
SCRIPT
mkdir -p "$T/home/inspector-check"
HOME="$T/home/inspector-check" CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/inspector-check.script" \
    "$BIN" "$CK" 2> "$T/inspector-check.err"
check_even="branch=main base=- ahead=0 behind=0 upstream=origin/main push=0 pull=0 dirty=0 changes=+0-0 tag=-"
check_vineyard="vineyard: $check_even check=none buttons=pull"
expect "$T/inspector-check-before.out" inspector "orchard: $check_even check=none buttons=pull | $check_vineyard"
expect "$T/inspector-check-after.out" inspector "orchard: branch=main base=- ahead=0 behind=0 upstream=origin/main push=0 pull=1 dirty=0 changes=+0-0 tag=- check=ok buttons=pull | $check_vineyard"
[ -f "$CK/orchard/pear.txt" ] || failed "inspector-check: Pull did not bring pear.txt"
[ -f "$CK/vineyard/pear.txt" ] && failed "inspector-check: orchard's Pull reached vineyard"
expect "$T/inspector-check-pulled.out" inspector "orchard: $check_even check=ok buttons=pull | $check_vineyard"
expect "$T/inspector-check-all.out" inspector "orchard: $check_even check=ok buttons=pull | vineyard: branch=main base=- ahead=0 behind=0 upstream=origin/main push=0 pull=1 dirty=0 changes=+0-0 tag=- check=ok buttons=pull"
