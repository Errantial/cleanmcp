# CleanMCP: notes for coding agents

TreeSize-style disk usage CLI for Windows + Linux with a built-in MCP server. Roadmap and design in `PLAN.md`.

## Layout
- `crates/cleanmcp-core`: scanning (`scan.rs`), tree types (`tree.rs`), queries (`query.rs`), cleanup rules (`rules.rs`). Library only; it must never delete files.
- `crates/cleanmcp`: the binary. `main.rs` (clap CLI), `output.rs` (text/JSON), `mcp.rs` (rmcp stdio server).

## Commands
- Build: `cargo build`
- Test: `cargo test`
- Lint (CI enforces): `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`
- Try the MCP server: `cargo run -q -- mcp` (the repo `.mcp.json` registers it for Claude Code)

## Rules
- Code must work on both Windows and Linux. Put OS-specific code behind `#[cfg(windows)]` / `#[cfg(unix)]` with a fallback.
- Never follow symlinks or junctions inside a scan (`DirEntry::metadata` does not follow them). The root path the user names is resolved with `resolve_path`; use the same function on any path you look up in a scan.
- MCP tools are read-only and marked `read_only_hint = true`. Any destructive tool must be opt-in via a server flag (see PLAN.md, Milestone 4).
- In `mcp` mode stdout is the protocol channel: log to stderr only, never `println!`.
- MCP tool results must be JSON objects (wrap lists in a struct).
- rmcp is 3.x; its API differs from older 0.x examples online. Check `~/.cargo/registry/src/*/rmcp-3.*/tests/` for current usage.
- Add a unit test with `tempfile` for any new scan/query/rule logic.
