<img src="assets/icon.svg" width="64" height="64" alt="">

# CleanMCP

A TreeSize-style disk usage explorer for the terminal on Windows and Linux, with a built-in [MCP](https://modelcontextprotocol.io) server so AI agents can find out what is filling your disk and suggest what to clean up.

## Install

**Windows** (PowerShell):

```powershell
irm https://raw.githubusercontent.com/Errantial/cleanmcp/main/install.ps1 | iex
```

**Linux** (x86_64 / arm64, static binary):

```sh
curl -fsSL https://raw.githubusercontent.com/Errantial/cleanmcp/main/install.sh | sh
```

Scoop: `scoop bucket add cleanmcp https://github.com/Errantial/cleanmcp; scoop install cleanmcp`.

Or download an archive from [Releases](https://github.com/Errantial/cleanmcp/releases), or build from
source: `cargo install --git https://github.com/Errantial/cleanmcp cleanmcp`.

Shell completions: `cleanmcp completions bash|zsh|fish|powershell` (also included in the archives).

## Usage

```sh
cleanmcp ui ~                          # interactive browser (mark + move to trash)
cleanmcp drives                        # volumes and free space
cleanmcp tree C:\Users\me -d 2         # size tree, largest first
cleanmcp files ~ --min-size 500MB --older-than 180
cleanmcp ext ~/Downloads               # usage by file type
cleanmcp age ~                         # usage by last-modified age
cleanmcp dupes ~/Pictures              # duplicate files
cleanmcp suggest ~                     # node_modules, target/, caches, temp...
cleanmcp snapshot ~ -o home.jsonl.gz   # save now...
cleanmcp diff home.jsonl.gz            # ...see what grew later
cleanmcp export ~ --format csv -o home.csv
cleanmcp tree . --json                 # most commands support --json
```

Sizes are space used on disk by default (`--apparent` for file lengths). Hard links are counted once
(`--dedupe-hardlinks` on Windows), symlinks/junctions are never followed, and OneDrive online-only
files count as 0 bytes on disk.

### Custom cleanup rules

`cleanmcp rules` lists the built-in rules. Add your own in `%APPDATA%\cleanmcp\config.toml` (Windows)
or `~/.config/cleanmcp/config.toml` (Linux):

```toml
exclude = ["**/.git"]          # always skip while scanning
disable_rules = ["xdg_cache"]  # turn off built-in rules by id

[[rule]]
id = "old_renders"
path = "**/Videos/renders"     # or: dir_name = "renders"
safety = "review"              # or "regenerable"
reason = "Rendered video output"
```

Only `cleanmcp ui` can delete, and only by moving items to the Recycle Bin / Trash after you confirm.
System folders, drive roots and your home folder are always refused. `suggest` lists folders with a reason and a safety level:
`regenerable` (build output, caches) or `review` (may hold data you want).

## MCP server

`cleanmcp mcp` runs an MCP server (stdio, or HTTP with `--http`) so agents like Claude can explore
your disks and suggest what to clean. Tools: `list_drives`, `scan_directory`, `list_children`,
`largest_files`, `extension_breakdown`, `age_breakdown`, `find_cleanup_candidates`, `find_duplicates`.
All read-only. `--allow-delete` adds `move_to_trash` (Recycle Bin only, refuses system folders,
audit-logged) and `--root DIR` limits what the agent can see.

```sh
claude mcp add --scope user cleanmcp -- cleanmcp mcp
```

Setup for Claude Desktop, VS Code and Cursor: [docs/mcp-setup.md](docs/mcp-setup.md).

## Building

```sh
cargo build --release      # binary at target/release/cleanmcp(.exe)
cargo test
cargo clippy --all-targets -- -D warnings
```

Requires Rust 1.85+. Roadmap and design notes: [PLAN.md](PLAN.md). Changes: [CHANGELOG.md](CHANGELOG.md).

## License

MIT or Apache-2.0, at your option.
