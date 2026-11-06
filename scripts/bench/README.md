# Benchmarks

Reproduce the numbers in [docs/benchmarks.md](../../docs/benchmarks.md):

- `bench-scan.ps1` (Windows) / `bench-scan.sh` (Linux/macOS): interleaved round-robin scan
  comparison of cleanmcp vs dua / dust / gdu on the same target — wall min/median, CPU min,
  peak memory. Put the competitor binaries on PATH (or next to the script) first.
- Query benchmarks on a scanned tree: `cargo run --release -p cleanmcp-core --example querybench -- <PATH>`
  (times `by_extension`, `by_age`, `largest_files`, `find_candidates` in-process, min of 3).
- Duplicate-finder walk: `cargo run --release -p cleanmcp-core --example dupes_bench -- <PATH> <MIN_SIZE>`.

Methodology (keep it consistent so numbers are comparable): one warmup run per tool, then N
interleaved runs on the same machine and day; report min of wall time and min of CPU seconds;
record machine, date, and tool versions in `docs/benchmarks.md`.
