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

# Every crate permitted to run a build script, and why. Adding a line here is
# a deliberate act: read the script first, then record what it does in
# docs/dependency-review.md in the same commit.
#
# Format: <name>-<version>  <build script path inside the crate>
ALLOWED_BUILD_SCRIPTS=$(cat <<'EOF'
objc2-0.6.4	build.rs
EOF
)

# Crates permitted to be proc macros. Empty on purpose.
ALLOWED_PROC_MACROS=$(cat <<'EOF'
EOF
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

# Dependencies only. The workspace's own build.rs has no `source`, and is
# reviewed in the diff like any other file in the repository.
deps='.packages[] | select(.source != null)'

fail=0

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
