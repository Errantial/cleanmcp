//! Temporary local benchmark harness for the duplicate finder (not committed).
//!
//! Usage: cargo run -r --example dupes_bench -- <PATH> <MIN_SIZE_BYTES>

use std::path::PathBuf;
use std::time::Instant;

use cleanmcp_core::dupes::{self, DupeOptions};
use cleanmcp_core::scan::{ScanOptions, scan};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = PathBuf::from(args.get(1).cloned().unwrap_or_else(|| ".".into()));
    let min_size: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1024);

    let t = Instant::now();
    let s = scan(&path, &ScanOptions::default()).unwrap();
    eprintln!(
        "scan: {:.2}s ({} entries)",
        t.elapsed().as_secs_f64(),
        count(&s.tree)
    );

    let t = Instant::now();
    let cands = dupes::same_size_files(&s.tree, &s.root, &DupeOptions { min_size });
    let walk = t.elapsed().as_secs_f64();

    let buckets = cands.len();
    let files: usize = cands.iter().map(|(_, v)| v.len()).sum();
    let bytes: u64 = cands.iter().map(|(sz, v)| sz * v.len() as u64).sum();
    let pairs = cands.iter().filter(|(_, v)| v.len() == 2).count();
    let small: usize = cands
        .iter()
        .filter(|(sz, _)| *sz <= 32768)
        .map(|(_, v)| v.len())
        .sum();
    eprintln!(
        "walk: {walk:.2}s  buckets {buckets}  files {files}  pairs-only buckets {pairs}  \
         bytes {:.1} GiB  files<=32K {small}",
        bytes as f64 / (1u64 << 30) as f64
    );

    let t = Instant::now();
    let groups = dupes::hash_candidates(cands);
    let hash = t.elapsed().as_secs_f64();
    let wasted: u64 = groups.iter().map(|g| g.wasted).sum();
    eprintln!(
        "hash: {hash:.2}s  groups {}  wasted {:.1} GiB",
        groups.len(),
        wasted as f64 / (1u64 << 30) as f64
    );
    eprintln!("total: {:.2}s", t.elapsed().as_secs_f64() + walk);
}

fn count(n: &cleanmcp_core::Node) -> u64 {
    let mut c = 1;
    for ch in n.children() {
        c += count(ch);
    }
    c
}
