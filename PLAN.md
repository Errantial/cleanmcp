# CleanMCP development plan

## Goal

One cross-platform binary (`cleanmcp`) for Windows and Linux that:

1. **Works like TreeSize in the terminal:** fast parallel scan, size tree sorted largest-first, largest files, per-extension breakdown, old-file filters, and later an interactive TUI.
2. **Has a built-in MCP server** (`cleanmcp mcp`) so AI agents can explore disk usage and suggest what to clean up, using the same engine as the CLI.

Guiding rule: **read-only by default.** Agents and users get evidence (sizes, ages, reasons). Deletion is a separate, explicit, opt-in step that moves files to the Recycle Bin / Trash and is never on by default for MCP.

## Tech choices

| Concern | Choice | Why |
|---|---|---|
| Language | Rust (edition 2024) | Single static binary per OS, fast filesystem walking, no runtime to install |
| CLI parsing | `clap` (derive) | Standard, generates help and shell completions |
| Parallel scan | `rayon` over `std::fs::read_dir` | Simple and fast; swap for `jwalk` or OS-specific APIs if profiling says so |
| MCP | `rmcp` 3.x (official Rust SDK), stdio transport | Official, macro-based tool definitions, JSON Schema via `schemars` |
| Output | Plain text tables + `--json` on every command | Humans and scripts/agents both covered |
| TUI (later) | `ratatui` + `crossterm` | Works in Windows Terminal and Linux terminals |
| Deletion (later) | `trash` crate | Recycle Bin on Windows, freedesktop Trash on Linux |

## Architecture

```
crates/
  cleanmcp-core/   library: no I/O except reading the filesystem, never deletes
    scan.rs        parallel scan -> Node tree (symlinks/junctions not followed)
    tree.rs        Node / Scan types, path lookup, walking
    query.rs       largest files, extension breakdown, age filters
    rules.rs       cleanup heuristics (node_modules, target, caches...) with safety levels
  cleanmcp/        binary
    main.rs        clap CLI: tree, files, ext, suggest, mcp
    output.rs      text + JSON rendering
    mcp.rs         MCP server; caches scans in memory per session
```

The CLI and MCP server both call `cleanmcp-core`, so their answers always match.

## Current state

- `cleanmcp tree [PATH] -d DEPTH -n TOP [--json]`
- `cleanmcp files [PATH] -n TOP [--min-size SIZE] [--older-than DAYS] [--newer-than DAYS] [--ext LIST] [--json]`
- `cleanmcp ext [PATH] -n TOP [--json]`
- `cleanmcp suggest [PATH] [--min-size SIZE] [--json]`
- `cleanmcp age|dupes|drives|rules|export|snapshot|diff` (Milestone 2)
- `cleanmcp ui` interactive browser with delete-to-trash (Milestone 3)
- `cleanmcp mcp [--root DIR] [--allow-delete] [--http ADDR]` with 8 read-only tools and an opt-in `move_to_trash` (Milestone 4)
- Unit tests for size roll-up and rule matching; CI on Windows + Linux

## Roadmap

### Milestone 1: solid scanner
- [x] Allocated size on disk vs logical size (cluster rounding + `GetCompressedFileSizeW` for compressed/sparse files on Windows, `st_blocks` on Linux); `--apparent` flag. Cloud placeholders (OneDrive online-only) count as 0 on disk
- [x] Hard-link de-duplication: always on Linux (free from `stat`); opt-in `--dedupe-hardlinks` on Windows (needs to open each file)
- [x] `--one-filesystem` (Linux `st_dev`; Windows never crosses mount points because they are reparse points)
- [x] Skip virtual filesystems on Linux by default (`/proc`, `/sys`, `/dev`, `/run`); `--include-virtual` to override
- [x] Exclude globs (`--exclude`). *Dropped "max depth during scan": it would make directory totals wrong; use `tree -d` to limit display depth instead.*
- [x] Progress line on stderr (entries, bytes, entries/s); `ScanControl` gives counters + cancel to library users (MCP). Ctrl-C in the CLI simply exits
- [x] Memory: compact `Node` with inline `CompactString` names instead of an arena: measured 101 B/entry peak on 8.6M entries (was 122); the post-0.1.0 pass shrank the node to 64 bytes and reached 83 B/entry (see `docs/benchmarks.md`)
- [x] Benchmarks recorded in `docs/benchmarks.md` (comparison with `dua`/`dust`/`gdu` deferred to the optimization pass)

### Milestone 2: TreeSize features
- [x] `cleanmcp drives`: volumes with size/used/free (via `sysinfo`; pseudo filesystems hidden unless `--all`)
- [x] `files` filters: `--min-size`, `--older-than`, `--newer-than` (days), `--ext`
- [x] `cleanmcp age`: usage by age bucket (TreeSize "age of files")
- [x] `cleanmcp dupes`: duplicate finder (size -> head/tail BLAKE3 -> full BLAKE3, parallel)
- [x] `cleanmcp export` (CSV/JSON), `cleanmcp snapshot` (JSON Lines, `.gz` optional) and `cleanmcp diff OLD [NEW]`
- [x] ~80 cleanup rules: project build output (Node, Rust, .NET, Gradle, Maven, Python, ...), Windows (Temp, Update cache, Windows.old, Recycle Bin, crash dumps, browser caches), Linux (journal, apt/dnf/pacman, snaps, Docker, Trash, ~/.cache), developer and AI caches (npm, pnpm, yarn, pip, cargo, Go, NuGet, Maven, Gradle, conda, Hugging Face, Ollama). Each has a safety level and, where useful, the proper cleanup command
- [x] User config (`config.toml`): extra rules, `disable_rules`, default `exclude`; `cleanmcp rules` lists what is active

### Milestone 3: interactive TUI
- [x] `cleanmcp ui [PATH]`: navigable tree with size bars and percentages; sort by size / file count / age / name; toggle disk vs apparent size; reveal in Explorer / file manager
- [x] Mark items (total shown), then move them to the Recycle Bin / Trash after a confirmation dialog; the tree updates in place
- [x] Protected paths (drive roots, your home folder and its parents, system folders) are refused by a shared check in `cleanmcp-core::safety`

### Milestone 4: MCP for agents
- [x] Resource `cleanmcp://scans` with the scans cached in the session
- [x] Progress notifications during `scan_directory` (when the client sends a progress token); cancellation stops the scan
- [x] Pagination (`offset` + `next_offset`) on `list_children`, `largest_files`, `find_cleanup_candidates`; small defaults and hard caps
- [x] New tools: `list_drives`, `age_breakdown`, `find_duplicates`; `largest_files` gained size/age/extension filters
- [x] `--root DIR` allow-list (paths resolved first, so `..` and links cannot escape)
- [x] Opt-in `move_to_trash` (`--allow-delete`): marked destructive, only accepts paths shown earlier in the session, refuses protected paths, logs every attempt to `deletions.log`; the tool is not even listed without the flag
- [x] Streamable HTTP transport (`--http ADDR`); localhost Host-header check against DNS rebinding
- [x] Setup docs for Claude Code, Claude Desktop, VS Code, Cursor (`docs/mcp-setup.md`)
- [x] End-to-end tests drive the real binary with the rmcp client (`crates/cleanmcp/tests/mcp_stdio.rs`)

### Milestone 5: distribution
- [x] Release workflow (`.github/workflows/release.yml`): on a `v*` tag, builds `x86_64-pc-windows-msvc`, `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl` on native runners, packages zip / tar.gz with completions, man page and licenses, publishes a GitHub release with SHA256SUMS and notes from CHANGELOG.md
- [x] Install scripts with checksum verification: `install.ps1` (Windows, adds to user PATH) and `install.sh` (Linux)
- [x] `cargo install --git`; Scoop bucket manifest in this repo; AUR `PKGBUILD` (cleanmcp-bin) in `packaging/aur`
- [x] Shell completions (`cleanmcp completions`) and man page (`cleanmcp man`)
- [ ] Not done (needs accounts/approval outside this repo): crates.io publish, winget-pkgs PR, AUR upload, Homebrew tap

## Safety principles

1. Core library never deletes.
2. MCP tools are read-only unless the server is started with an explicit flag.
3. Deletion goes to the Recycle Bin / Trash, never permanent, and never follows links.
4. Every suggestion carries a reason and a safety level (`regenerable` vs `review`).
5. System locations (`C:\Windows`, `/usr`, `/etc`, ...) are never suggested for deletion.

## Platform notes

- **Windows:** junctions and symlinks are reparse points; `symlink_metadata` reports them as links, so we do not follow them. Long paths (>260 chars) work through `std` (it uses `\\?\` internally). Some folders need admin rights; they are counted in `errors`, not fatal. Later: NTFS MFT reading for very fast whole-drive scans (needs admin; optional).
- **Linux:** do not cross into `/proc` or `/sys`; handle permission errors; respect `st_blocks` for sparse files.
