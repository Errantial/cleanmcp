# Benchmarks

Machine: Windows 11, 24 logical CPUs, NVMe SSD. Release build. "Warm" = second run, OS file cache populated.

## Milestone 1 (scanner)

Target: `C:\Users\<me>` with 7.75M files, 0.88M dirs, 600 GiB.

| Version | Warm | Cold | Peak memory | Bytes/entry |
|---|---|---|---|---|
| v0.1 scaffold (`Vec<Node>`, `String` names, `symlink_metadata` per entry) | 126 s | 343 s | 1,052 MB | 122 |
| M1 (compact 80-byte node, inline names, no per-file syscalls) | **17.8 s** | 116 s | **871 MB** | **101** |

What made the difference:
- OneDrive "online-only" placeholders: asking Windows for their allocated size goes through the cloud
  filter driver (~200 us/file). They are now counted as 0 bytes on disk from their attributes. 196k files: 41.6 s -> 1.0 s.
- `FILE_ATTRIBUTE_RECALL_ON_OPEN` has the same value as `FILE_ATTRIBUTE_EA`, which ordinary files have
  (e.g. after WSL touched them). Treating it as "special" caused one extra syscall per file. 1M files: 17.2 s -> 2.5 s.
- `CompactString` names (inline up to 24 bytes) remove one heap allocation per entry.

Comparisons with `dua`, `dust`, `gdu` are in the v0.6.0 section below.

## v0.6.0 (query parallelism + competitor comparison)

Machine: Windows 11, 24 logical CPUs, NVMe SSD. Release build. "Warm" = after a warmup run,
tools run interleaved (round-robin) so OS cache and machine load hit every tool equally.

### Scan vs dua / dust / gdu (whole-tree scan, wall time min of runs)

| Target | cleanmcp 0.5.0 | dua v3.0.0 | dust v1.2.7 | gdu v5.38.0 |
|---|---|---|---|---|
| `C:\Users\Emil\AppData` (957k entries, 172 GiB) | **2.99 s** | 3.52 s | 39.97 s | 3.90 s |
| `C:\Users\Emil` (6.1M entries, 577 GiB) | 21.22 s | 21.20 s | 311.19 s | **20.69 s** |

CPU seconds (min) and peak working set on the same runs:

| Target | cleanmcp | dua | dust | gdu |
|---|---|---|---|---|
| AppData | cpu 36.2 s, 128 MB | cpu 48.2 s, 90 MB | cpu 344 s, 383 MB | cpu 45.4 s, 82 MB |
| Profile | cpu 241 s, 531 MB | cpu 269 s, 317 MB | cpu 2167 s, 2199 MB | cpu 272 s, 58 MB |

cleanmcp is the fastest on the smaller tree and at parity with dua/gdu on the big profile,
with the lowest CPU of the three fast tools. It uses more memory than dua (~87 vs ~52 bytes
per entry: 64-byte nodes with inline names plus one heap allocation per long name, where dua
stores names in an arena). dust is an order of magnitude slower on Windows; gdu's peak looks
low because Go returns memory to the OS between runs.

### Queries on a scanned tree (in-process, min of 3, `querybench` example)

v0.6.0 walks every whole-tree query in parallel with per-chunk accumulators merged up the
tree (no shared lock, no per-file allocation), instead of one mutex or a single thread.

| Query | v0.5.0 profile | v0.6.0 profile | v0.5.0 AppData | v0.6.0 AppData |
|---|---|---|---|---|
| `by_extension` (`ext`) | 3050 ms | **130 ms** (23x) | 607 ms | **90 ms** (6.7x) |
| `by_age` (`age`) | 281 ms | **17 ms** (16x) | 40 ms | **7 ms** (5.7x) |
| `largest_files` (`files`) | 282 ms | **44 ms** (6.4x) | 42 ms | **20 ms** (2.1x) |
| `find_candidates` (`suggest`) | 274 ms | **30 ms** (9x) | 56 ms | **19 ms** (2.9x) |

Profile = `C:\Users\Emil` (5.45M files, 676k dirs); AppData = `C:\Users\Emil\AppData`
(853k files, 105k dirs). The scan itself is unchanged (2.5 s on AppData in both versions);
the MCP tools share these functions, so they speed up the same way on cached scans.

## Optimization pass (after 0.1.0)

Same target (`C:\Users\<me>`, 7.86M files, 0.88M dirs, 630 GiB). Both binaries were built from
the same tree and run back to back on the same day, so the comparison is like-for-like even
though absolute times on this machine were higher than the M1 run above (disk and antivirus
state differ).

| Version | In-process scan | Peak memory | Bytes/entry |
|---|---|---|---|
| v0.1.0 | 113–135 s | 873 MB | 100 |
| Optimized | **68–98 s** | **721 MB** | **83** |

What made the difference:

- **64-byte nodes** (was 80): counters packed into the existing `meta` word, `mtime` stored as
  `u32` seconds, and children held as a length-prefixed heap slice instead of a `Box<[Node]>`
  fat pointer. Pre-1970 timestamps clamp to the epoch.
- **Directory enumeration**: one `read_dir` pass that reuses a single path buffer instead of
  allocating a `PathBuf` per entry, and per-directory batching of the progress counters so
  24 threads stop contending on the same cache line.
- **Duplicate finder**: size histogram first, then streaming fingerprints in bounded memory,
  instead of collecting every candidate path before hashing.
- **Snapshots and export**: a hand-written JSON row writer and chunked output buffers remove
  `serde_json` per-row overhead; rows stay byte-identical to the 0.1.0 format.
- **Queries and rules**: one `Matcher` per walk (clock read once, extensions lowercased once)
  and a literal-component anchor that skips the glob engine for almost every directory.
