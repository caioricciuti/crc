#!/usr/bin/env bash
# Verifies that what was copied into the repository from elsewhere is what
# was reviewed.
#
# third_party/ holds C compiled into the binary by our own build.rs, which
# is outside everything the cargo gates look at. tests/spec/ holds the
# official WebAssembly spec tests the interpreter is checked against. Each
# folder's CHECKSUMS pins every file by SHA-256, and its SOURCES.md says
# which upstream commit they came from and which differ from it on purpose.
#
# A change to a vendored file therefore shows up twice in a diff: once in a
# file nobody can review by reading (parser.c is 206K generated lines), and
# once as a changed line here, which is the one a reviewer will actually see.
#
# Usage: scripts/check-third-party.sh                   verify both folders
#        scripts/check-third-party.sh --update <folder> rewrite its CHECKSUMS
#                                                       after a deliberate
#                                                       re-vendor
set -euo pipefail
cd "$(dirname "$0")/.."

list() {
    # .DS_Store: Finder writes one into any folder it is shown.
    find . -type f ! -name CHECKSUMS ! -name SOURCES.md ! -name .DS_Store | LC_ALL=C sort
}

if [ "${1:-}" = "--update" ]; then
    folder=${2:?"which folder: third_party or tests/spec"}
    cd "$folder"
    list | xargs shasum -a 256 > CHECKSUMS
    echo "wrote $folder/CHECKSUMS ($(wc -l < CHECKSUMS | tr -d ' ') files)"
    echo "Update $folder/SOURCES.md in the same commit."
    exit 0
fi

for folder in third_party tests/spec; do
    (
        cd "$folder"
        # A file that was added without being listed is as interesting as
        # one that changed, and `shasum -c` alone only looks at what is
        # listed.
        if ! diff <(list) <(awk '{print $2}' CHECKSUMS | LC_ALL=C sort) >/dev/null; then
            echo "::error::$folder/ does not hold exactly the files in CHECKSUMS"
            diff <(list) <(awk '{print $2}' CHECKSUMS | LC_ALL=C sort) || true
            exit 1
        fi
        if ! shasum -a 256 -c --quiet CHECKSUMS; then
            echo "::error::a file in $folder/ differs from its reviewed checksum"
            exit 1
        fi
        echo "$folder: $(wc -l < CHECKSUMS | tr -d ' ') files match their checksums"
    )
done
