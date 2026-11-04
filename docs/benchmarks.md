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
