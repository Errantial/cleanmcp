<img src="assets/icon.svg" width="64" height="64" alt="">

# CleanMCP

A TreeSize-style disk usage explorer for the terminal on Windows and Linux, with a built-in [MCP](https://modelcontextprotocol.io) server so AI agents can find out what is filling your disk and suggest what to clean up.

## Install

**Windows** (PowerShell):

```powershell
irm https://raw.githubusercontent.com/Errantial/cleanmcp/main/install.ps1 | iex
```

**Linux / macOS** (x86_64 / arm64, static binary):

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

Sizes are space used on disk by default (`--apparent` for logical file sizes). Hard links are
counted once on Linux; pass `--dedupe-hardlinks` to do the same on Windows (slower). Symlinks and
junctions inside the tree are never followed (the scan root you pass is resolved first), and
OneDrive online-only files count as 0 bytes on disk. `.gitignore` / `.ignore` files are respected
while scanning (`--no-ignore` to disable), and `cleanmcp du PATH` prints `du`-compatible output.

## How does it compare?

| | cleanmcp | dua | dust | gdu | ncdu | TreeSize |
|---|---|---|---|---|---|---|
| Platforms | Windows, Linux, macOS | Win/Linux/macOS | Win/Linux/macOS | Win/Linux/macOS | Linux/macOS | Windows |
| Scan speed (1M+ entries) | fastest or parity — see [benchmarks](docs/benchmarks.md) | fast | slow on Windows | fast | n/a | GUI |
| Interactive TUI | `cleanmcp ui` | yes | no | yes | yes | GUI |
| Delete to trash | `ui`, MCP opt-in | yes | no | yes | yes | yes |
| `.gitignore` respected | yes (`--no-ignore`) | no | yes | yes | no | filters |
| Duplicate finder | `dupes` | no | no | no | no | yes |
| Age / extension breakdowns | `age`, `ext` | partial | yes | yes | no | yes |
| Cleanup suggestions with safety levels | `suggest` (~80 rules) | no | no | no | no | partial |
| MCP server for AI agents | yes (8 tools + opt-in trash) | no | no | no | no | no |
| Snapshots & diff over time | `snapshot`, `diff` | no | no | export/import | export/import | reports |

Benchmark methodology and numbers: [docs/benchmarks.md](docs/benchmarks.md) (reproduce with
`scripts/bench/`). Competitor flags verified at v1.0.0; tell us if the table is stale.

## FAQ

**Why not just `dua`/`dust`/`gdu`?** They are great scanners. cleanmcp adds what they don't have:
a duplicate finder, ~80 cleanup rules with safety levels, snapshots you can diff over time, a
`du`-compatible mode, and an MCP server so an AI agent can explore your disk and suggest (never
perform, unless you opt in) cleanup. On raw scan speed we are at parity or faster
([benchmarks](docs/benchmarks.md)).

**Is it safe to let an agent use it?** The MCP server is read-only by default. `move_to_trash`
only exists with `mcp --allow-delete`, only accepts paths a tool returned in that session, always
refuses drive roots / system folders / your home folder / other profiles, moves to the Recycle
Bin or Trash (never permanent), never follows links, and logs every attempt. `--root DIR` limits
what the tools may touch. See [SECURITY.md](SECURITY.md).

**Why does a scan report "N entries could not be read"?** Some folders need admin rights. Those
entries are counted and reported (stderr note, `tree --json` `errors` field, MCP scan result)
instead of being silently skipped, so totals you see are honest.

**Windows vs Linux differences?** Sizes are allocated-on-disk by default on both. Linux counts
hard links once for free; on Windows that needs `--dedupe-hardlinks` (slower). Linux skips
`/proc`, `/sys`, `/dev`, `/run` unless `--include-virtual`; `--one-filesystem` (`-x`) stays on
one filesystem. Windows never crosses mount points (they are reparse points, never followed).

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

`cleanmcp ui` moves items to the trash after you confirm, and `cleanmcp mcp --allow-delete` lets an
agent do the same for paths a tool returned in that session. Either way the item goes to the
Recycle Bin / Trash, never removed permanently; a symlink is trashed as a link, not followed to its
target. Drive roots, system folders, your home folder (and any folder containing it) and other
users' profiles are always refused. `suggest` lists folders with a reason and a safety level:
`regenerable` (build output, caches) or `review` (may hold data you want).

## MCP server

`cleanmcp mcp` runs an MCP server (stdio, or HTTP with `--http`) so agents like Claude can explore
your disks and suggest what to clean. Tools: `list_drives`, `scan_directory`, `list_children`,
`largest_files`, `extension_breakdown`, `age_breakdown`, `find_cleanup_candidates`, `find_duplicates`.
All read-only. `--allow-delete` adds `move_to_trash`: it only accepts paths a tool returned in that
session, still refuses drive roots, system folders, your home folder, other profiles and any
`--root` folder, and logs every attempt that reaches the trash step. `--root DIR` limits which paths
the tools may access. HTTP has no authentication, so `--allow-delete` is refused on a non-loopback
`--http` address.

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

Requires Rust 1.88+. Roadmap and design notes: [PLAN.md](PLAN.md). Changes: [CHANGELOG.md](CHANGELOG.md).

## License

MIT or Apache-2.0, at your option.
