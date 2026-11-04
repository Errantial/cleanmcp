//! Temporary A/B harness: old vs new duplicate finder in one process (not committed).
//!
//! Usage: cargo run -r --example dupes_ab -- <PATH> <MIN_SIZE_BYTES> [ROUNDS]

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Instant;

use rayon::prelude::*;

use cleanmcp_core::dupes::{self, DupeOptions, DuplicateGroup};
use cleanmcp_core::scan::{ScanOptions, scan};
use cleanmcp_core::tree::{Node, NodeKind};

// ---------------------------------------------------------------- old code (v0.1.0)

mod old {
    use super::*;

    const PARTIAL: u64 = 16 * 1024;

    pub fn same_size_files(root: &Node, base: &Path, opts: &DupeOptions) -> Vec<(u64, Vec<PathBuf>)> {
        let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
        root.walk(base, &mut |p, n| {
            if n.kind == NodeKind::File && n.size >= opts.min_size.max(1) {
                by_size.entry(n.size).or_default().push(p.to_path_buf());
            }
        });
        by_size.into_iter().filter(|(_, v)| v.len() > 1).collect()
    }

    pub fn hash_candidates(candidates: Vec<(u64, Vec<PathBuf>)>) -> Vec<DuplicateGroup> {
        let mut groups: Vec<DuplicateGroup> = candidates
            .into_par_iter()
            .flat_map_iter(|(size, paths)| {
                let partial = regroup(paths, |p| hash_partial(p, size));
                partial
                    .into_iter()
                    .flat_map(move |(key, ps)| {
                        let full = if size <= 2 * PARTIAL {
                            vec![(key, ps)]
                        } else {
                            regroup(ps, hash_full)
                        };
                        full.into_iter().map(move |(hash, mut paths)| {
                            paths.sort();
                            DuplicateGroup {
                                size,
                                wasted: size * (paths.len() as u64 - 1),
                                hash,
                                paths,
                            }
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        groups.sort_unstable_by(|a, b| b.wasted.cmp(&a.wasted).then_with(|| a.paths.cmp(&b.paths)));
        groups
    }

    fn regroup(
        paths: Vec<PathBuf>,
        key: impl Fn(&Path) -> io::Result<String> + Sync,
    ) -> Vec<(String, Vec<PathBuf>)> {
        let keyed: Vec<(String, PathBuf)> = paths
            .into_par_iter()
            .filter_map(|p| key(&p).ok().map(|k| (k, p)))
            .collect();
        let mut map: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for (k, p) in keyed {
            map.entry(k).or_default().push(p);
        }
        map.into_iter().filter(|(_, v)| v.len() > 1).collect()
    }

    fn hash_partial(path: &Path, size: u64) -> io::Result<String> {
        let mut f = File::open(path)?;
        let mut h = blake3::Hasher::new();
        let mut buf = vec![0u8; PARTIAL as usize];
        let n = read_up_to(&mut f, &mut buf)?;
        h.update(&buf[..n]);
        if size > 2 * PARTIAL {
            f.seek(SeekFrom::End(-(PARTIAL as i64)))?;
            let n = read_up_to(&mut f, &mut buf)?;
            h.update(&buf[..n]);
        } else if size > PARTIAL {
            let n = read_up_to(&mut f, &mut buf)?;
            h.update(&buf[..n]);
        }
        Ok(h.finalize().to_hex().to_string())
    }

    fn hash_full(path: &Path) -> io::Result<String> {
        let mut h = blake3::Hasher::new();
        h.update_reader(File::open(path)?)?;
        Ok(h.finalize().to_hex().to_string())
    }

    fn read_up_to(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
        let mut total = 0;
        while total < buf.len() {
            match f.read(&mut buf[total..])? {
                0 => break,
                n => total += n,
            }
        }
        Ok(total)
    }
}

fn count(n: &Node) -> u64 {
    let mut c = 1;
    for ch in &n.children {
        c += count(ch);
    }
    c
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = PathBuf::from(args.get(1).cloned().unwrap_or_else(|| ".".into()));
    let min_size: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1024);
    let rounds: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(2);

    let t = Instant::now();
    let s = scan(&path, &ScanOptions::default()).unwrap();
    println!("scan {:.2}s ({} entries)", t.elapsed().as_secs_f64(), count(&s.tree));

    let opts = DupeOptions { min_size };
    let t = Instant::now();
    let old_cands = old::same_size_files(&s.tree, &s.root, &opts);
    let old_walk = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let new_cands = dupes::same_size_files(&s.tree, &s.root, &opts);
    let new_walk = t.elapsed().as_secs_f64();
    let files: usize = new_cands.iter().map(|(_, v)| v.len()).sum();
    let bytes: u64 = new_cands.iter().map(|(sz, v)| sz * v.len() as u64).sum();
    println!(
        "candidates: old walk {old_walk:.2}s / new walk {new_walk:.2}s, {} buckets, {files} files, {:.1} GiB",
        new_cands.len(),
        bytes as f64 / (1u64 << 30) as f64,
    );

    let mut old_groups = Vec::new();
    let mut new_groups = Vec::new();
    for round in 0..rounds {
        let t = Instant::now();
        old_groups = old::hash_candidates(old_cands.clone());
        let a = t.elapsed().as_secs_f64();
        let t = Instant::now();
        new_groups = dupes::hash_candidates(new_cands.clone());
        let b = t.elapsed().as_secs_f64();
        println!("round {}: old {a:.2}s  new {b:.2}s  ({:.2}x)", round + 1, a / b);
    }

    let key = |g: &DuplicateGroup| {
        let mut paths = g.paths.clone();
        paths.sort();
        format!("{}|{}|{:?}", g.size, g.hash, paths)
    };
    let mut ka: Vec<String> = old_groups.iter().map(key).collect();
    let mut kb: Vec<String> = new_groups.iter().map(key).collect();
    ka.sort();
    kb.sort();
    println!(
        "groups: old {} new {}  identical: {}",
        ka.len(),
        kb.len(),
        ka == kb
    );
    if ka != kb {
        for (a, b) in ka.iter().zip(&kb) {
            if a != b {
                println!("  first diff:\n   old {a}\n   new {b}");
                break;
            }
        }
    }
}
