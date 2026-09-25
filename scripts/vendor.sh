#!/usr/bin/env bash
# Vendors every dependency locally and points cargo at it.
#
# Not required to build: `cargo build --locked` works straight from a clone.
# Use this when you want a fully offline build, or when auditing exactly what
# source goes into the binary.
#
# Neither vendor/ nor .cargo/config.toml is committed; both are gitignored.
set -euo pipefail

cd "$(dirname "$0")/.."

echo "==> vendoring"
mkdir -p .cargo
# --locked: vendor exactly what Cargo.lock names, and fail rather than
# quietly rewrite the lockfile if the two have drifted.
cargo vendor --locked --versioned-dirs > .cargo/config.toml

echo
echo "==> build scripts, proc macros, tree size"
# Cargo's own list of what it would run, not a guess from file names.
./scripts/check-build-scripts.sh

echo
echo "==> $(ls vendor | wc -l | tr -d ' ') crates vendored; builds are now offline"
