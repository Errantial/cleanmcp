# Changelog

## 0.1.0 - 2026-11-04

First release: a TreeSize-style disk usage tool for the terminal on Windows and Linux, with a
built-in MCP server so AI agents can find what to clean up.

### Command line
- `tree`, `files`, `ext`, `age`: size tree, largest files (size / age / type filters), usage by
  extension and by age. Sizes are space on disk by default (`--apparent` for file lengths).
- `ui`: interactive browser with size bars and sorting; mark items and move them to the
  Recycle Bin / Trash after confirming.
- `suggest` and `rules`: ~80 built-in cleanup rules (build output, dependency folders, package
  manager and browser caches, Windows Update cache, temp folders, Docker, AI model caches...),
  each with a safety level and the proper cleanup command. Add your own in `config.toml`.
- `dupes`: duplicate files (size, then partial and full BLAKE3 hashes).
- `drives`, `export` (CSV / JSON), `snapshot` + `diff` to see what grew.
- `completions` for bash, zsh, fish, PowerShell, elvish.

### MCP server (`cleanmcp mcp`)
- Tools: `list_drives`, `scan_directory` (cached, with progress and cancel), `list_children`,
  `largest_files`, `extension_breakdown`, `age_breakdown`, `find_cleanup_candidates`,
  `find_duplicates`. All read-only.
- `--allow-delete` adds `move_to_trash` (Recycle Bin only, audit-logged, refuses system folders);
  `--root` limits access; `--http` serves Streamable HTTP.

### Accuracy and speed
- Parallel scanner; symlinks and junctions are never followed; hard links counted once;
  OneDrive online-only files count as 0 bytes on disk; compressed and sparse files measured.
- About 485k entries/s on a warm Windows profile scan, about 83 bytes of memory per entry.

### Downloads
- `x86_64-pc-windows-msvc` (zip), `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`
  (static, tar.gz). Each archive includes shell completions and a man page.
