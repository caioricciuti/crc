#!/usr/bin/env bash
# Verifies that no crate in the dependency tree runs code at build or compile
# time without having been reviewed.
#
# Cargo has no allowlist for this. A build script runs arbitrary code during
# `cargo build`, and a proc macro runs arbitrary code during compilation,
# neither with any sandbox and neither opt-out-able. This script is the
# allowlist.
#
# It asks cargo, through `cargo metadata`, which targets it would build,
# rather than reading manifests itself. Two earlier versions of this check
# each trusted a proxy and each had a hole:
#
#   - globbing for a file named build.rs misses a crate that points `build`
#     somewhere else, which two of the first crates we evaluated did
#     (`binding_rust/build.rs`, `bindings/rust/build.rs`);
#   - grepping manifests for a `build = ` line misses the opposite case, a
#     crate with no `build` key at all that ships a build.rs, which cargo
#     detects on its own and runs. It also missed `build="x"`,
#     `package.build`, `proc_macro = true` and `crate-type = ["proc-macro"]`.
#
# Cargo's own answer has neither problem, because it is the list cargo acts on.
#
# Usage: scripts/check-build-scripts.sh
#        scripts/check-build-scripts.sh --from <metadata.json>   (for testing)
# Requires: jq, and vendor/ to exist (run scripts/vendor.sh first), so that
# the answer describes the vendored sources and needs no network.
set -uo pipefail
cd "$(dirname "$0")/.."

# ---- what has been reviewed (docs/dependency-review.md) ----------------------
# Adding a line to any list here is a deliberate act: read the code first,
# then record what it does in docs/dependency-review.md in the same commit.

# Every dependency, <name>-<version>, from crates.io.
ALLOWED_CRATES=$(cat <<'LIST'
bitflags-2.13.2
cc-1.4.5
dispatch2-0.3.1
find-msvc-tools-0.1.13
objc2-0.6.4
objc2-app-kit-0.3.2
objc2-core-foundation-0.3.2
objc2-core-graphics-0.3.2
objc2-core-text-0.3.2
objc2-encode-4.1.0
objc2-foundation-0.3.2
objc2-metal-0.3.2
objc2-quartz-core-0.3.2
shlex-2.0.1
LIST
)

# Dependencies permitted to run a build script: <name>-<version>, then the
# script's path inside the crate.
ALLOWED_BUILD_SCRIPTS=$(cat <<'LIST'
objc2-0.6.4	build.rs
LIST
)

# Dependencies permitted to be proc macros. Empty on purpose.
ALLOWED_PROC_MACROS=$(cat <<'LIST'
LIST
)

# This workspace's own build scripts and proc macros: <package>, then path.
# crc's build.rs compiles the vendored C; it is reviewed in the diff.
ALLOWED_WORKSPACE_BUILD=$(cat <<'LIST'
crc	build.rs
LIST
)

# More crates than this needs an argument in docs/dependency-review.md.
MAX_CRATES=20

if ! command -v jq >/dev/null; then
    echo "error: jq is required." >&2
    exit 2
fi

if [ "${1:-}" = "--from" ]; then
    metadata=$(cat "$2") || exit 2
else
    if [ ! -d vendor ]; then
        echo "error: vendor/ missing. Run scripts/vendor.sh first." >&2
        exit 2
    fi
    # --locked: a vendor/ that has drifted from Cargo.lock is an error here,
    # not a tree that gets approved by mistake.
    if ! metadata=$(cargo metadata --format-version 1 --locked --offline); then
        echo "::error::cargo metadata failed. Is vendor/ in step with Cargo.lock?"
        exit 2
    fi
fi

# Dependencies: every package with a source. The workspace's own packages
# have none and are checked on their own below.
deps='.packages[] | select(.source != null)'
here=$(pwd -P)

fail=0

# ---- where every package comes from ---------------------------------------
# crates.io only: a git or alternative-registry dependency is refused, since
# it can change under the same version.
foreign=$(printf '%s' "$metadata" | jq -r "$deps"'
    | select(.source != "registry+https://github.com/rust-lang/crates.io-index")
    | "\(.name)-\(.version)\t\(.source)"')
if [ -n "$foreign" ]; then
    echo "::error::dependencies from outside crates.io"
    printf '%s\n' "$foreign"
    fail=1
else
    echo "sources: crates.io only"
fi

# Exactly the crates reviewed, at the versions reviewed. A new crate or a
# bump fails here until it is read and added to ALLOWED_CRATES.
actual_crates=$(printf '%s' "$metadata" | jq -r "$deps"' | "\(.name)-\(.version)"' | sort)
expected_crates=$(printf '%s\n' "$ALLOWED_CRATES" | grep -v '^$' | sort)
if [ "$expected_crates" != "$actual_crates" ]; then
    echo "::error::the dependency tree is not the reviewed one"
    echo "--- allowed ---"
    printf '%s\n' "${expected_crates:-(none)}"
    echo "--- actual ---"
    printf '%s\n' "${actual_crates:-(none)}"
    echo
    echo "Review each new crate (docs/dependency-review.md), then add it to"
    echo "ALLOWED_CRATES in this file in the same commit."
    fail=1
else
    echo "crates: $(printf '%s\n' "$actual_crates" | grep -c .), all reviewed"
fi

# Packages without a source are path packages. Each must be a member of
# this workspace, inside this repository: a path dependency elsewhere on the
# disk would bring code no review of this repository covers.
strays=$(printf '%s' "$metadata" | jq -r --arg here "$here/" '
    .workspace_members as $members
    | .packages[] | select(.source == null)
    | select((.id as $id | $members | index($id) | not)
             or ((.manifest_path | startswith($here)) | not))
    | "\(.name)\t\(.manifest_path)"')
if [ -n "$strays" ]; then
    echo "::error::path packages outside this workspace"
    printf '%s\n' "$strays"
    fail=1
else
    echo "path packages: workspace members only"
fi

# The workspace's own build scripts and proc macros run too, at the first
# cargo command a pull request reaches.
actual_own=$(printf '%s' "$metadata" | jq -r '
    .packages[] | select(.source == null)
    | . as $p
    | (.manifest_path | sub("/Cargo\\.toml$"; "/")) as $root
    | .targets[] | select((.kind | index("custom-build")) or (.kind | index("proc-macro")))
    | "\($p.name)\t\(.src_path | ltrimstr($root))"' | sort)
expected_own=$(printf '%s\n' "$ALLOWED_WORKSPACE_BUILD" | grep -v '^$' | sort)
if [ "$expected_own" != "$actual_own" ]; then
    echo "::error::the workspace's own build scripts or proc macros changed"
    echo "--- allowed ---"
    printf '%s\n' "${expected_own:-(none)}"
    echo "--- actual ---"
    printf '%s\n' "${actual_own:-(none)}"
    fail=1
else
    echo "workspace build scripts: ${actual_own:-none}, as allowlisted"
fi

# ---- build scripts -------------------------------------------------------
actual=$(printf '%s' "$metadata" | jq -r "$deps"'
    | . as $p
    | (.manifest_path | sub("/Cargo\\.toml$"; "/")) as $root
    | .targets[] | select(.kind | index("custom-build"))
    | "\($p.name)-\($p.version)\t\(.src_path | ltrimstr($root))"' | sort)
expected=$(printf '%s\n' "$ALLOWED_BUILD_SCRIPTS" | grep -v '^$' | sort)

if [ "$expected" != "$actual" ]; then
    echo "::error::build script inventory does not match the allowlist"
    echo
    echo "--- allowed ---"
    printf '%s\n' "$expected"
    echo "--- actual ---"
    printf '%s\n' "$actual"
    echo
    echo "New entries run arbitrary code during every build, on every"
    echo "contributor's machine and in CI. Read each one, then add it to"
    echo "ALLOWED_BUILD_SCRIPTS in this file and describe it in"
    echo "docs/dependency-review.md."
    fail=1
else
    count=$(printf '%s\n' "$actual" | grep -c . || true)
    echo "build scripts: $count, all allowlisted"
fi

# ---- proc macros ---------------------------------------------------------
actual_macros=$(printf '%s' "$metadata" | jq -r "$deps"'
    | . as $p
    | select(any(.targets[]; .kind | index("proc-macro")))
    | "\($p.name)-\($p.version)"' | sort)
expected_macros=$(printf '%s\n' "$ALLOWED_PROC_MACROS" | grep -v '^$' | sort)

if [ "$expected_macros" != "$actual_macros" ]; then
    echo "::error::proc-macro inventory does not match the allowlist"
    echo "--- allowed ---"
    printf '%s\n' "${expected_macros:-(none)}"
    echo "--- actual ---"
    printf '%s\n' "$actual_macros"
    echo
    echo "A proc macro executes during compilation and cannot be opted out of."
    fail=1
else
    echo "proc macros: ${actual_macros:-none}, as allowlisted"
fi

# ---- tripwire on tree size ----------------------------------------------
count=$(printf '%s' "$metadata" | jq "[$deps] | length")
echo "crates in the tree: $count"
if [ "$count" -gt "$MAX_CRATES" ]; then
    echo "::error::dependency count is $count. Justify it in docs/dependency-review.md and raise MAX_CRATES deliberately."
    fail=1
fi

exit $fail
