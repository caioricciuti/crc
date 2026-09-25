#!/usr/bin/env bash
# Manual crates.io vetting before adding a crate.
# Reports, per crate: version that would install, publish date, age in days,
# downloads, maintainer count. Read-only API queries, no code execution.
set -uo pipefail

UA="crc-vet/0.1 (manual dependency review)"
TODAY=$(date -u +%s)

for pkg in "$@"; do
  meta=$(curl -sS -H "User-Agent: $UA" "https://crates.io/api/v1/crates/${pkg}" 2>/dev/null)
  if [ -z "$meta" ] || [ "$(printf '%s' "$meta" | jq -r 'has("crate")')" != "true" ]; then
    printf '%-22s  NOT FOUND or API error\n' "$pkg"
    continue
  fi

  # newest non-yanked version
  read -r ver created dl <<<"$(printf '%s' "$meta" | jq -r '
    [.versions[] | select(.yanked == false)] | .[0]
    | "\(.num) \(.created_at) \(.downloads)"')"

  total=$(printf '%s' "$meta" | jq -r '.crate.downloads')
  created_epoch=$(date -u -j -f "%Y-%m-%dT%H:%M:%S" "${created%%.*}" +%s 2>/dev/null || echo "$TODAY")
  age_days=$(( (TODAY - created_epoch) / 86400 ))

  owners=$(curl -sS -H "User-Agent: $UA" "https://crates.io/api/v1/crates/${pkg}/owners" 2>/dev/null \
    | jq -r '[.users[]?.login] | join(", ")' 2>/dev/null)
  [ -z "$owners" ] && owners="(unknown)"

  printf '%-22s v%-12s published %s (%3sd ago)\n' "$pkg" "$ver" "${created:0:10}" "$age_days"
  printf '%-22s   downloads: %s this version / %s total\n' "" "$dl" "$total"
  printf '%-22s   maintainers: %s\n' "" "$owners"
  echo
done
