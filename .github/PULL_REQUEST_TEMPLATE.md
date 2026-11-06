## What does this PR change?

<!-- One or two sentences. If it fixes an issue, mention it: "Fixes #123" -->

## Checklist

- [ ] `cargo test` passes (new behavior has tests — CLI behavior in `crates/cleanmcp/tests/cli.rs`)
- [ ] `cargo clippy --all-targets -- -D warnings` is clean
- [ ] `cargo fmt --all --check` is clean
- [ ] `CHANGELOG.md` has an entry under "Unreleased" if user-visible
- [ ] Snapshot format unchanged, or old snapshots still load (`#[serde(default)]`) — say which
- [ ] Performance impact measured with `scripts/bench/` if it touches scan/query paths
- [ ] Safety rules respected: core never deletes; MCP read-only unless `--allow-delete`;
      `--root` resolves paths before checking; protected paths refused
