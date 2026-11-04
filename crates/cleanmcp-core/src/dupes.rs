//! Duplicate file finder: group by size, then by a partial hash, then by a full hash.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use serde::Serialize;

use crate::tree::{Node, NodeKind};

/// Bytes hashed from the start and the end of a file in the quick pass.
const PARTIAL: u64 = 16 * 1024;

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DuplicateGroup {
    /// Size of each copy in bytes.
    pub size: u64,
    /// Space freed by keeping one copy: `size * (copies - 1)`.
    pub wasted: u64,
    /// BLAKE3 hash of the content (hex).
    pub hash: String,
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct DupeOptions {
    /// Ignore files smaller than this (bytes). Small files are many and save little.
    pub min_size: u64,
}

impl Default for DupeOptions {
    fn default() -> Self {
        DupeOptions {
            min_size: 1024 * 1024,
        }
    }
}

/// Find files below `base` with identical content, most wasted space first.
///
/// Files that cannot be read are skipped. On Windows, hard links show up as
/// duplicates unless the scan used `dedupe_hardlinks`.
pub fn find_duplicates(root: &Node, base: &Path, opts: &DupeOptions) -> Vec<DuplicateGroup> {
    // 1. Group by size (cheap, from the scan).
    let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    root.walk(base, &mut |p, n| {
        if n.kind == NodeKind::File && n.size >= opts.min_size.max(1) {
            by_size.entry(n.size).or_default().push(p.to_path_buf());
        }
    });
    let candidates: Vec<(u64, Vec<PathBuf>)> =
        by_size.into_iter().filter(|(_, v)| v.len() > 1).collect();

    // 2. Partial hash (head + tail), then 3. full hash, both in parallel.
    let mut groups: Vec<DuplicateGroup> = candidates
        .into_par_iter()
        .flat_map_iter(|(size, paths)| {
            let partial = regroup(paths, |p| hash_partial(p, size));
            partial
                .into_iter()
                .flat_map(move |(key, ps)| {
                    let full = if size <= 2 * PARTIAL {
                        // The partial hash already covered the whole file.
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

/// Split `paths` by `key`, keeping only groups of 2 or more. Unreadable files are dropped.
fn regroup(
    paths: Vec<PathBuf>,
    key: impl Fn(&Path) -> io::Result<String> + Sync,
) -> Vec<(String, Vec<PathBuf>)> {
    // Hash in parallel: one size bucket can hold thousands of files.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{ScanOptions, scan};
    use std::fs;

    #[test]
    fn finds_identical_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let big: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(p.join("a"), &big).unwrap();
        fs::create_dir(p.join("sub")).unwrap();
        fs::write(p.join("sub/b"), &big).unwrap();
        // Same size, same head and tail, different middle: must not match.
        let mut other = big.clone();
        other[50_000] ^= 0xFF;
        fs::write(p.join("c"), &other).unwrap();
        // Small duplicates (whole file covered by the partial hash).
        fs::write(p.join("s1"), b"hello world").unwrap();
        fs::write(p.join("s2"), b"hello world").unwrap();

        let s = scan(p, &ScanOptions::default()).unwrap();
        let g = find_duplicates(&s.tree, &s.root, &DupeOptions { min_size: 1 });
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].size, 100_000);
        assert_eq!(g[0].wasted, 100_000);
        assert_eq!(g[0].paths.len(), 2);
        assert!(g[0].paths.iter().all(|p| !p.ends_with("c")));
        assert_eq!(g[1].paths.len(), 2);
    }
}
