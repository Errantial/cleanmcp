<img src="assets/icon.svg" width="64" height="64" alt="">

# CleanMCP

A TreeSize-style disk usage explorer for the terminal on Windows and Linux, with a built-in [MCP](https://modelcontextprotocol.io) server so AI agents can find out what is filling your disk and suggest what to clean up.

> Early development. See [PLAN.md](PLAN.md) for the roadmap.

## Usage

```sh
cleanmcp tree C:\Users\me -d 2        # size tree, largest first
cleanmcp files ~ -n 20 --older-than-days 180
cleanmcp ext ~/Downloads              # usage by file extension
cleanmcp suggest ~/code               # node_modules, target/, caches...
cleanmcp tree . --json                # every command supports --json
```

Nothing is ever deleted. `suggest` lists folders with a reason and a safety level:
`regenerable` (build output, caches) or `review` (may hold data you want).

## MCP server

`cleanmcp mcp` runs an MCP server over stdio with read-only tools:

| Tool | What it does |
|---|---|
| `scan_directory` | Scan a folder/drive, return total size and largest children (cached) |
| `list_children` | Drill into a directory, sorted by size |
| `largest_files` | Biggest files, optionally only old ones |
| `extension_breakdown` | Usage per file type |
| `find_cleanup_candidates` | Folders that are usually safe to remove, with reasons |

Claude Code:

```sh
claude mcp add cleanmcp -- cleanmcp mcp
```

Claude Desktop / other clients (`mcpServers` config):

```json
{ "mcpServers": { "cleanmcp": { "command": "cleanmcp", "args": ["mcp"] } } }
```

## Building

```sh
cargo build --release      # binary at target/release/cleanmcp(.exe)
cargo test
cargo clippy --all-targets -- -D warnings
```

Requires Rust 1.85+.
