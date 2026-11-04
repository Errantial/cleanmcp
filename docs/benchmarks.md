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

Comparisons with `dua`, `dust`, `gdu` are pending (see the optimization pass).

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
