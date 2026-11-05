# Changelog

## 0.6.0 - 2026-11-04

Parallel whole-tree queries, benchmarked against `dua`, `dust` and `gdu`. No breaking changes;
commands, flags and the snapshot format are unchanged.

### Speed
- Every whole-tree query now walks the tree in parallel with per-chunk accumulators that are
  merged up the tree, instead of one shared lock or a single thread. Measured in-process
  (min of 3) on `C:\Users\Emil` (5.45M files, 676k dirs) and `C:\Users\Emil\AppData`
  (853k files): `ext` 3050 -> 130 ms (23x) and 607 -> 90 ms (6.7x), `age` 281 -> 17 ms and
  40 -> 7 ms, `files` 282 -> 44 ms and 42 -> 20 ms, `suggest` 274 -> 30 ms and 56 -> 19 ms.
  `by_extension` also no longer allocates a `String` per file: a repeat extension is found
  with a borrowed `&str` and allocates nothing. `dupes` counts sizes and collects candidate
  paths in parallel too. The MCP tools share these functions, so they speed up the same way
  on cached scans. Details and methodology: `docs/benchmarks.md`.
- Benchmarked against dua v3.0.0, dust v1.2.7 and gdu v5.38.0 (Windows x86_64, warm cache,
  interleaved runs): on `C:\Users\Emil\AppData` (957k entries, 172 GiB) cleanmcp scans in
  2.99 s vs dua 3.52 s, gdu 3.90 s, dust 39.97 s; on `C:\Users\Emil` (6.1M entries, 577 GiB)
  21.2 s vs dua 21.2 s, gdu 20.7 s, dust 311 s, with the lowest CPU of the three fast tools
  (241 s vs 269/272 s on the profile). Peak memory is ~87 bytes/entry vs dua's ~52.

### CLI
- `files --sort age` now sorts files with an unknown modification time last instead of
  first: "oldest first" should not lead with files whose age is unknown.

## 0.5.0 - 2026-11-04

Faster breakdowns on big trees, and small CLI/MCP conveniences. One breaking change: the shape
of `suggest --json`, called out under CLI; the snapshot format is unchanged.

### Speed
- `ext` and `age` now walk the tree in parallel (rayon) instead of single-threaded. On a
  249k-file tree the aggregation itself drops from seconds to milliseconds; the scan still
  dominates wall time. `by_age` accumulates into per-bucket atomics, `by_extension` into one
  locked table (few distinct extensions, so contention is low).

### CLI
- **Breaking:** `suggest --json` is now an object with `total_count`, `total_size`,
  `total_size_human` and `candidates` (previously a bare array), matching the MCP tool's totals.
- `files` gains `--sort size|name|age` (default `size`). The top `--top` files are still picked
  by size; the flag only orders the rows shown.

### MCP server
- `list_drives` takes an optional `include_virtual` flag to also list pseudo filesystems
  (tmpfs, squashfs, ...), matching the CLI's `drives --all`.

## 0.4.0 - 2026-11-04

Agent-facing improvements: every MCP read tool can now report apparent (logical) sizes, and two
new tools let an agent snapshot a folder and diff it later. No breaking changes; the snapshot
format is unchanged.

### MCP server
- New `save_snapshot` tool: scan a folder and write a snapshot file (`.gz` = compressed) for later
  comparison. The output file must be inside the server's `--root` folders, and the parent folder
  is what gets root-checked (the file itself does not exist yet).
- New `diff_snapshots` tool: compare a snapshot with a newer snapshot, a folder scanned now, or the
  snapshot's own root rescanned. Reports what grew or shrank, largest absolute change first. The
  "scan now" side always rescans, so a cached scan from an earlier tool call cannot make a diff
  report nothing changed.
- `scan_directory`, `list_children`, `largest_files`, `extension_breakdown`,
  `find_cleanup_candidates` and `find_duplicates` take an optional `apparent` flag to report and
  sort by logical file sizes instead of space used on disk (matching the CLI's `--apparent`).
  `find_duplicates` prices wasted space in the selected mode.

## 0.3.0 - 2026-11-04

Security hardening, more honest JSON, a layout for narrow terminals, and faster diffs. The one
breaking change is the shape of `dupes --json`, called out under CLI; the snapshot format is
unchanged.

### Speed
- `diff` loads snapshots up to **5x faster** by skipping rows below the requested depth (1.7x end
  to end on a 288k-entry snapshot; output unchanged). A 288,135-entry / 13.4 MB snapshot loads in
  20.5 ms instead of 106 ms at depth 1 and 25 ms instead of 132 ms at depth 3 (gzip: 64/67 ms,
  was 155/217 ms), with allocations down from 62,375 to 16 and from 62,535 to 864. `cleanmcp
  diff -d 3` on that snapshot is 167 ms instead of 290 ms plain and 308 ms instead of 406 ms
  gzipped. Full loads still parse and validate every row.

### Safety
- `move_to_trash` now re-applies `--root` and the "only paths this session was shown" rule to the
  exact path handed to the trash crate. The parent is resolved twice, so a folder swapped for a
  link between the check and the delete is refused instead of silently deleted.
- Path identity on Windows is component-wise case-insensitive, matching the rest of the scan, so a
  path retyped in another case cannot slip past the `--root` or seen-path checks. The same
  identity is used for tree lookup, removal and the `--root` guard.
- Refuses to trash the Recycle Bin and freedesktop Trash stores and their contents
  (`C:\$Recycle.Bin`, any volume's `$Recycle.Bin`, `System Volume Information`,
  `~/.local/share/Trash`, `/Trash`): they hold data that is already deleted or still recoverable.
- Bounds what a client can make `move_to_trash` do: 256 paths per call, 128 KiB per path string,
  4 KiB per reason, and 100k remembered paths. Nested targets are pruned against every accepted
  parent, and a path is reported deleted only when its parent actually moved.
- Other users' profiles are protected on relocated layouts (Windows `D:\Profiles\me`, Silverblue's
  `/var/home/me`), not only under the hardcoded `C:\Users` / `/home` roots.
- The test that asserts home folders are protected no longer calls the real trash on your home
  directory; a regression now fails an assertion instead of binning it.

### MCP server
- A client cancel, dropped request, or server shutdown now stops the scan behind every tool, not
  only `scan_directory`; a `spawn_blocking` scan that nobody is waiting for is cancelled.
- `freed_disk_size` and the deletion audit log take the size from the most specific cached scan,
  not the least recently used one (a refreshed subtree could report the parent's stale size).
- A parent scan that started earlier no longer evicts a newer subtree scan when it finishes later.

### Snapshots
- Loading a snapshot rolls up per-node error counts again: a file that could not be read was
  invisible to its ancestors' totals (released snapshots carry own-errors, so this affects them).
- A snapshot truncated between rows, or with a folder whose size no longer matches its children,
  now fails to load instead of producing a plausible partial tree that `diff` reports as fiction.
- Rejects a file containing two roots (concatenated snapshots) and children under a file row.
- `save` fsyncs before the rename, so a crash cannot leave an empty snapshot where a good one was.
- A scan root that is not valid UTF-8 is stored lossily instead of failing the save on Linux.

### Windows
- Small files on network drives are no longer reported as 0 bytes on disk (NTFS "resident file"
  accounting is only applied to local volumes).
- Fixed the `\\?\UNC\` prefix built for UNC paths, which was missing its separator.
- Added tests that a junction is classified as a link and is never descended, and that the
  `\\?\` prefixes are well formed.

### CLI
- `dupes` now prices each copy and the wasted total in the selected size mode: the default reports
  space used on disk (matching the rest of the tool) and `--apparent` reports logical lengths.
  Grouping still uses the logical length, which identical content requires.
- **Breaking:** `dupes --json` is now an object with `total_groups`, `total_wasted` and `groups`
  instead of a bare array, so a truncated page no longer hides how many groups exist or what they
  total. Every group also reports `disk_size` and `wasted_disk_size`; the MCP `find_duplicates`
  groups gain those two fields (additive) and keep their logical totals.
- `tree --json` nodes gained `children_truncated` (the depth limit or `top` left children out, so
  an empty `children` list no longer looks like an empty folder) and `other_size` (the omitted
  children in the selected size mode). Omitted-children totals now also cover folders below the
  depth limit, not only the tail cut by `top`. `list_children` in the MCP server shares this shape,
  so it gains both fields and reports the omitted children at its depth limit too.
- An explicit `--config` path that does not exist now errors instead of silently using defaults.
- An invalid `--exclude` glob now says the pattern is invalid instead of "cannot scan <path>",
  which blamed the folder for a typo.
- `diff <directory>` now says it needs a snapshot file instead of "cannot read <dir>: access is
  denied".

### Terminal UI
- Below 60 columns the tree list switches to a filename-first row with one right-aligned
  size/percent column, and the header elides the path from the left so the folder name survives.
- Footer hints are chosen from what fits and always keep `? help` and `q quit`. The help popup has
  a one-column layout that fits 40x10, and drops its cosmetic blank line before dropping hints.

### Build
- `rust-version` is 1.88 (the code uses let-chains, stabilized there) and CI compiles that version.

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
