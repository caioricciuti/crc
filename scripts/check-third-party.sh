#!/usr/bin/env bash
# Verifies that the C checked into third_party/ is the C that was reviewed.
#
# Those files are compiled into the binary by our own build.rs, so they are
# outside everything the cargo gates look at. third_party/CHECKSUMS pins each
# one by SHA-256, and third_party/SOURCES.md says which upstream commit they
# came from and which of them differ from it on purpose.
#
# A change to a vendored file therefore shows up twice in a diff: once in a
# file nobody can review by reading (parser.c is 206K generated lines), and
# once as a changed line here, which is the one a reviewer will actually see.
#
# Usage: scripts/check-third-party.sh            verify
#        scripts/check-third-party.sh --update   rewrite CHECKSUMS after a
#                                                deliberate re-vendor
set -euo pipefail
cd "$(dirname "$0")/../third_party"

list() {
    # .DS_Store: Finder writes one into any folder it is shown.
    find . -type f ! -name CHECKSUMS ! -name SOURCES.md ! -name .DS_Store | LC_ALL=C sort
}

if [ "${1:-}" = "--update" ]; then
    list | xargs shasum -a 256 > CHECKSUMS
    echo "wrote third_party/CHECKSUMS ($(wc -l < CHECKSUMS | tr -d ' ') files)"
    echo "Update third_party/SOURCES.md in the same commit."
    exit 0
fi

# A file that was added without being listed is as interesting as one that
# changed, and `shasum -c` alone only looks at what is listed.
if ! diff <(list) <(awk '{print $2}' CHECKSUMS | LC_ALL=C sort) >/dev/null; then
    echo "::error::third_party/ does not hold exactly the files in CHECKSUMS"
    diff <(list) <(awk '{print $2}' CHECKSUMS | LC_ALL=C sort) || true
    exit 1
fi

if ! shasum -a 256 -c --quiet CHECKSUMS; then
    echo "::error::a vendored file differs from its reviewed checksum"
    exit 1
fi
echo "third_party: $(wc -l < CHECKSUMS | tr -d ' ') files match their checksums"
