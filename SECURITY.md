# Security

## Reporting a vulnerability

Please report it privately, not in a public issue: on GitHub, open the
repository's **Security** tab and choose **Report a vulnerability**. Only
the maintainer sees the report. You will get an answer within a few days,
and credit in the fix's release notes unless you would rather not.

Useful to include: the version (`crc --version`), macOS version, what an
attacker needs (a file opened in crc, a cloned repository, a local
process), and the smallest steps that show it.

## Supported versions

crc is in alpha. Fixes go into the next release; only the newest release
is supported.

## What is in scope

Anything that lets content or another program do more than it should:

- Opening a file, folder or repository in crc running code, reading files
  outside it, or corrupting other files.
- The Claude Code bridge: it listens on `127.0.0.1` only, behind a random
  token in a lock file readable by your user alone. A way for another
  process or a web page to reach it without that token is a vulnerability.
- The terminal, `.http` requests (run through `/usr/bin/curl`) and the
  language servers crc starts.
- The update check, which only reads GitHub's list of releases and opens a
  page; it never downloads or installs anything.
- The release pipeline: a way to get unsigned or altered code into a
  published release.

## How releases can be checked

Every release is built by the workflow in `.github/workflows/release.yml`
from a signed tag. The app and `crc.dmg` are signed with a Developer ID
and notarized by Apple, and each release carries `SHA256SUMS`:

```sh
shasum -a 256 -c SHA256SUMS
spctl -a -vv -t open --context context:primary-signature crc.dmg
```

Third-party code is kept small and read before it is added; see
[docs/dependency-review.md](docs/dependency-review.md).
