# Security policy

## Reporting a vulnerability

Please report security issues **privately** — do not open a public issue.

- Use GitHub's private vulnerability reporting: https://github.com/Errantial/cleanmcp/security/advisories/new
- Or email the maintainer (see the GitHub profile of @Errantial).

You should receive a response within 72 hours. If the issue is confirmed, we will develop a fix,
release it, and credit you in the release notes (unless you prefer not to be named).

## What counts as a security issue

cleanmcp is read-only by default, but it does have a deletion path (`ui`, and `mcp --allow-delete`)
and an MCP server that agents drive. In scope:

- **Path escapes**: anything that lets `--root DIR` be bypassed (`..`, symlinks, junctions,
  case tricks, UNC paths) so a tool reads or trashes outside the allow-list.
- **Trash escapes**: `move_to_trash` following a symlink to its target, or trashing a protected
  path (drive roots, system folders, home folder, other users' profiles).
- **MCP transport**: the HTTP mode's localhost Host-header check being bypassable (DNS rebinding),
  or `--allow-delete` being accepted on a non-loopback address.
- **Install scripts**: `install.ps1` / `install.sh` executing or installing something other than
  the checksum-verified release artifact.
- **Supply chain**: a dependency advisory that affects us (CI runs `cargo deny check`), or a
  release artifact whose SHA256SUMS / attestation does not verify.
- **Snapshot parsing**: a malicious snapshot file causing memory exhaustion or path confusion in
  `diff`/`export`.

Out of scope: permission-denied errors during scans (counted and reported, not a vulnerability),
slow scans, or feature requests.

## Safe harbor

If you report in good faith and follow the above, we will not pursue legal action against you for
testing the released binaries of this project.
