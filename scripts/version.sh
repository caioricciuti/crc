#!/usr/bin/env bash
# Prints crc's version, from Cargo.toml: the one place it is written.
set -euo pipefail
cd "$(dirname "$0")/.."
grep -m1 '^version = ' Cargo.toml | sed 's/version = //; s/"//g'
