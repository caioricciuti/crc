#!/usr/bin/env bash
# Walks a test instance through the app's surfaces and keeps a frame of
# each, in the dark and the light theme, so a change to the chrome can be
# judged by looking rather than by reading code. The frames are drawn by
# the renderer itself (the self-test's `frame` step), not captured from
# the screen, so this needs no permission and the window need not be in
# front.
#
# Usage: scripts/ui-tour.sh [--debug] [--out DIR] [words...]
#   --out DIR   where the PNGs go (default: a new folder under /tmp, printed)
#   --debug     use the debug build
#   words       keep only the frames whose names contain one of the words
#               (the whole tour still runs; it is the cheap part)
#
# Isolation is the self-test's: an empty HOME per theme, the run's own
# history database and Claude config folder, /bin/sh as the terminal shell.
# The fixture is an invented crate with one dirty Markdown file and one
# dirty Rust file, so Git, history and the editor have something to show.
set -euo pipefail
cd "$(dirname "$0")/.."

debug=0
out=""
while [ $# -gt 0 ]; do
    case "$1" in
        --debug) debug=1 ;;
        --out) shift; out=$1 ;;
        *) break ;;
    esac
    shift
done
if [ "$debug" = 1 ]; then
    cargo build --locked
    BIN=target/debug/crc
else
    cargo build --release --locked
    BIN=target/release/crc
fi
BIN="$PWD/$BIN"
T=$(mktemp -d)
[ -n "$out" ] || out=$(mktemp -d /tmp/crc-ui-tour.XXXXXX)
mkdir -p "$out" "$T/home" "$T/claude-config"
export CLAUDE_CONFIG_DIR="$T/claude-config"
export CRC_HISTORY_DB="$T/history.db"
export CRC_TERMINAL_SHELL=/bin/sh
unset XDG_CONFIG_HOME
trap 'pkill -f -- "--hold $T/home/" 2>/dev/null || true; rm -rf "$T"' EXIT

# ---- fixture ---------------------------------------------------------------
P="$T/garden"
mkdir -p "$P/src" "$P/docs"
cat > "$P/Cargo.toml" <<'EOF'
[package]
name = "garden"
version = "0.3.1"
edition = "2021"
EOF
cat > "$P/src/main.rs" <<'EOF'
//! Garden planner: a toy program for screenshots.
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
struct Plant {
    name: String,
    days_to_harvest: u32,
    sun: Sun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sun {
    Full,
    Partial,
    Shade,
}

fn plan(plants: &[Plant]) -> BTreeMap<u32, Vec<&str>> {
    let mut by_week = BTreeMap::new();
    for plant in plants {
        let week = plant.days_to_harvest / 7;
        by_week.entry(week).or_insert_with(Vec::new).push(plant.name.as_str());
    }
    by_week
}

fn main() {
    let plants = vec![
        Plant { name: "tomato".into(), days_to_harvest: 75, sun: Sun::Full },
        Plant { name: "lettuce".into(), days_to_harvest: 45, sun: Sun::Partial },
        Plant { name: "mint".into(), days_to_harvest: 60, sun: Sun::Shade },
    ];
    for (week, names) in plan(&plants) {
        println!("week {week}: {}", names.join(", "));
    }
}
EOF
cat > "$P/src/lib.rs" <<'EOF'
pub fn area(width: f64, height: f64) -> f64 {
    width * height
}

#[cfg(test)]
mod tests {
    use super::area;

    #[test]
    fn rectangle() {
        assert_eq!(area(2.0, 3.0), 6.0);
    }
}
EOF
cat > "$P/docs/notes.md" <<'EOF'
# Garden notes

A **planner** for a small balcony garden.

## Beds

| Bed | Sun | Plants |
|-----|-----|--------|
| A   | full | tomato, basil |
| B   | partial | lettuce |

- [x] order seeds
- [ ] build the second bed

```rust
let week = days / 7;
```
EOF
cat > "$P/docs/harvest.csv" <<'EOF'
date,plant,grams,notes
2026-06-02,lettuce,120,"first cut, very tender"
2026-06-09,lettuce,95,
2026-07-14,tomato,410,"two split in the rain"
2026-07-21,mint,30,dried
EOF
printf 'target/\n' > "$P/.gitignore"
g() { git -C "$P" -c user.name=Tester -c user.email=t@example.com -c commit.gpgsign=false "$@"; }
g init -q
g add -A
g commit -q -m "feat: first planner"
printf '\n## Next\n\nTry peppers.\n' >> "$P/docs/notes.md"
sed -i '' 's/days_to_harvest: 60/days_to_harvest: 65/' "$P/src/main.rs"

# ---- the tour --------------------------------------------------------------
# Every surface is reached by its own key or toolbar control, the way a
# person reaches it. The file-opening steps after the Extensions page land
# in the pane behind it (see the plan's UI item 4); they are kept so that
# fix shows up here too.
tour() {
    local theme=$1
    local home="$T/home/$theme"
    mkdir -p "$home/.config/crc"
    printf 'theme = "%s"\n' "$theme" > "$home/.config/crc/config.toml"
    cat > "$T/$theme.script" <<SCRIPT
resize 1440 900
wait 600
frame $out/$theme-01-open.bmp
key 11 cmd b
wait 300
frame $out/$theme-02-sidebar-hidden.bmp
key 35 cmd p
wait 200
text main
wait 300
frame $out/$theme-03-palette-files.bmp
key 36
wait 500
frame $out/$theme-04-editor.bmp
key 35 cmd p
wait 200
text >
wait 300
frame $out/$theme-05-palette-commands.bmp
key 53
wait 100
key 3 cmd,shift f
wait 200
text plant
wait 900
frame $out/$theme-06-project-search.bmp
key 53
wait 200
key 5 cmd,opt g
wait 900
frame $out/$theme-07-git.bmp
key 5 cmd,opt g
wait 200
key 16 cmd,opt y
wait 900
frame $out/$theme-08-file-history.bmp
key 53
wait 300
key 42 cmd \\
wait 400
frame $out/$theme-09-split.bmp
click @toolbar.terminal
wait 1200
frame $out/$theme-10-terminal.bmp
click @toolbar.terminal
wait 300
key 43 cmd ,
wait 400
frame $out/$theme-11-settings.bmp
key 53
wait 200
key 7 cmd,shift x
wait 1200
frame $out/$theme-12-extensions.bmp
key 53
wait 200
key 35 cmd p
wait 200
text notes.md
wait 300
key 36
wait 400
key 14 cmd e
wait 900
frame $out/$theme-13-markdown-preview.bmp
key 14 cmd e
wait 200
key 35 cmd p
wait 200
text harvest
wait 300
key 36
wait 400
frame $out/$theme-14-csv.bmp
key 0 cmd,shift A
wait 1200
frame $out/$theme-15-agents.bmp
click @toolbar.inspector
wait 1200
frame $out/$theme-16-inspector.bmp
quit
SCRIPT
    HOME="$home" CRC_SELFTEST="$T/$theme.script" "$BIN" "$P" 2> "$T/$theme.err" \
        || echo "crc exited $? in the $theme tour: $(tail -3 "$T/$theme.err")"
}

tour dark
tour light

# The home screen: no path and an empty home.
mkdir -p "$T/home/start/.config/crc"
printf 'theme = "dark"\n' > "$T/home/start/.config/crc/config.toml"
printf 'resize 1440 900\nwait 800\nframe %s/dark-00-home.bmp\nquit\n' "$out" > "$T/start.script"
HOME="$T/home/start" CRC_SELFTEST="$T/start.script" "$BIN" 2> "$T/start.err" \
    || echo "crc exited $? on the home screen: $(tail -3 "$T/start.err")"

for f in "$out"/*.bmp; do
    [ -e "$f" ] || continue
    sips -s format png "$f" --out "${f%.bmp}.png" >/dev/null 2>&1 && command rm -f "$f"
done
if [ $# -gt 0 ]; then
    for f in "$out"/*.png; do
        keep=0
        for word in "$@"; do
            case "$(basename "$f")" in *"$word"*) keep=1 ;; esac
        done
        [ "$keep" = 1 ] || command rm -f "$f"
    done
fi
echo "Frames: $out"
ls "$out"
