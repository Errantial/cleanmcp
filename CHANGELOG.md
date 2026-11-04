# Changelog

## 0.2.0 - 2026-11-04

Performance pass, plus correctness fixes found by reviewing the optimized code. No breaking
changes; commands, flags and the snapshot format are unchanged.

### Speed and memory
- Scans are about **1.7x faster** and use **17% less memory**: 68-98 s and 721 MB instead of
  113-135 s and 873 MB on the 7.86M-entry benchmark tree (83 bytes per entry, was 100).
- `Node` shrank from 80 to 64 bytes (packed counters, `u32` mtime, length-prefixed child list).
- Directory enumeration reuses one path buffer and batches progress counters per directory.
- The duplicate finder streams fingerprints instead of collecting every candidate first.
- Snapshots and exports use a hand-written JSON writer and chunked buffers; rows are
  byte-identical to 0.1.0's.
- CSV/JSON export no longer allocates a `PathBuf` per row.

### Fixes
- A folder that exists but cannot be listed now fails with a non-zero exit instead of reporting
  a confident "0 B".
- `suggest --apparent` now filters and reports apparent sizes instead of silently using disk
  sizes (previously it could report nothing for cloud-only files).
- The MCP server picks the most specific cached scan: `scan_directory(sub, refresh=true)` is no
  longer shadowed by an older scan of its parent, and the cache evicts least-recently-used.
- `scan_directory` no longer answers a `one_filesystem` question with a scan made without it.
- The duplicate finder no longer reports a group from a stale scan-time size when a file has
  grown past the quick-hash window.
- Cleanup rules with a non-ASCII path component (e.g. `**/Téléchargements/*`) match again.
- `snapshot -o` writes to a temp file and renames it, so a failed save cannot destroy the
  previous snapshot.
- `stats.cancelled` now means "totals are partial" rather than "someone pressed stop", so a
  cancel racing the last directory no longer discards a complete MCP scan.
- On Windows, a directory listing that fails before yielding anything now falls back to `std`
  instead of silently losing the subtree.

### MCP server
- `find_duplicates` gained `offset`/`next_offset`, like the other list tools.
- `scan_directory` now reports excluded entries, skipped mounts, deduplicated hard links,
  cancellation, `one_filesystem`, and how many children were left out of `top_children`.
- `list_children` lowers `top` when `depth > 1` so one response stays under ~20k rows.

### Terminal UI
- The header shows how many entries could not be read.
- The footer picks the most detailed key hints that fit, so `? help` and `q quit` survive an
  80-column terminal.
- The delete confirmation elides long paths and always shows the `y = yes` hint.
- PageUp/PageDown move one screen instead of a fixed 15 rows.
- Fixed double-encoded box-drawing and status glyphs that printed as mojibake.

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
