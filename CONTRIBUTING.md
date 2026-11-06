# Contributing to cleanmcp

Thanks for helping. This project is a single Rust workspace; contributions of any size are
welcome — bug reports, cleanup-rule ideas, docs, benchmarks, and code.

## Development setup

Requires Rust 1.88+ (the MSRV, checked in CI).

```sh
git clone https://github.com/Errantial/cleanmcp.git
cd cleanmcp
cargo build --release      # binary at target/release/cleanmcp(.exe)
cargo test                 # unit + integration tests (drives the real binary)
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
cargo deny check           # licenses + advisories (install: cargo install cargo-deny)
```

Layout: `crates/cleanmcp-core` is the library (scan, tree, queries, rules, snapshots — it never
deletes anything); `crates/cleanmcp` is the binary (CLI, TUI, MCP server). The CLI and MCP server
both call the core, so their answers always match — keep it that way.

## Pull requests

- Keep PRs small and focused; one change per PR.
- Add or update tests for behavior changes (`cargo test` must pass; new CLI behavior belongs in
  `crates/cleanmcp/tests/cli.rs`, core behavior in the module's `#[cfg(test)]`).
- Run `cargo fmt --all` and `cargo clippy --all-targets -- -D warnings` before pushing.
- Add a `CHANGELOG.md` entry under an "Unreleased" heading if the change is user-visible.
- If you change the snapshot format, keep old snapshots readable (`#[serde(default)]`) and say
  so in the PR description — snapshots are a compatibility surface.
- Performance changes: include a before/after number from `scripts/bench/` (see
  `docs/benchmarks.md` for methodology) when you can.

## Adding a cleanup rule

Rules live in `crates/cleanmcp-core/src/rules.rs` (`builtin_rules`). Each rule has an `id`, a
match (`path` glob or `dir_name`), a `safety` level (`regenerable` or `review`), a `reason`, and
optionally `advice`. Prefer `regenerable` only for things that are truly re-creatable (build
output, caches). Add a unit test in `rules.rs` for the new rule.

## Safety rules (non-negotiable)

1. The core library never deletes.
2. MCP tools are read-only unless the server runs with `--allow-delete`.
3. Deletion goes to the Recycle Bin / Trash, never permanent, and never follows links.
4. System locations (`C:\Windows`, `/usr`, `/etc`, ...) are never suggested for deletion.
5. `--root` allow-lists must resolve paths before checking, so `..` and links cannot escape.

## Reporting bugs

Use the bug report template. Include the command you ran, the output (or `--json` output), your
OS, and `cleanmcp --version`. For scan discrepancies, a `cleanmcp snapshot` of the folder helps.
