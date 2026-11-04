//! Duplicate file finder: group by size, then by a quick head/tail hash, then by
//! a full hash.
//!
//! The work is done in three passes:
//!
//! 1. **Sizes** (from the scan tree, no I/O): collect files of equal size, ignoring
//!    sizes that only one file has. Paths are only built for files that survive.
//! 2. **Quick hash** (parallel, 32 KiB per file): the first and last block. Files
//!    that are no bigger than [`SMALL`] are hashed completely here instead, because
//!    a second open + read would cost more than hashing the bytes once.
//! 3. **Full hash** (parallel): groups that still match are hashed completely.
//!    Groups of exactly two files are compared byte by byte instead, which stops at
//!    the first difference and hashes one of them on the way.
//!
//! Unreadable files (locked, deleted, permission denied) are dropped: they never
//! form a group and never abort the run.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use blake3::Hash;
use rayon::prelude::*;
use serde::Serialize;

use crate::tree::{Node, NodeKind};

/// Bytes hashed from the start and the end of a file in the quick pass.
const PARTIAL: u64 = 16 * 1024;
/// Files up to this size are hashed completely in the quick pass: that covers the
/// whole file, so no second pass is needed. Keeping it at two blocks (and not
/// higher) matters: hashing mid-sized files completely in the quick pass reads a
/// lot more when their head blocks differ already.
const SMALL: u64 = 2 * PARTIAL;
const _: () = assert!(SMALL >= 2 * PARTIAL);
/// Read buffer for the full pass and for byte comparison.
const BUF: usize = 256 * 1024;

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
    hash_candidates(same_size_files(root, base, opts))
}

/// Files of equal size, the cheap first step of [`find_duplicates`]. Only this
/// step needs the scan tree, so callers can release locks before hashing.
///
/// Sizes that only one file has are dropped before any path is built: on a big
/// tree most files are unique, so this saves one allocation per file.
pub fn same_size_files(root: &Node, base: &Path, opts: &DupeOptions) -> Vec<(u64, Vec<PathBuf>)> {
    let min = opts.min_size.max(1);
    let mut counts: HashMap<u64, u32> = HashMap::new();
    count_sizes(root, min, &mut counts);
    counts.retain(|_, n| *n > 1);
    if counts.is_empty() {
        return Vec::new();
    }
    let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::with_capacity(counts.len());
    let mut path = base.to_path_buf();
    collect_paths(root, min, &counts, &mut path, &mut by_size);
    // Biggest files first: the hashing pass is I/O bound and this order gives the
    // long reads to the pool before the short ones.
    let mut out: Vec<(u64, Vec<PathBuf>)> = by_size.into_iter().collect();
    out.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    out
}

fn count_sizes(node: &Node, min: u64, counts: &mut HashMap<u64, u32>) {
    for c in &node.children {
        match c.kind {
            NodeKind::File => {
                if c.size >= min {
                    *counts.entry(c.size).or_default() += 1;
                }
            }
            NodeKind::Dir => count_sizes(c, min, counts),
            _ => {}
        }
    }
}

fn collect_paths(
    node: &Node,
    min: u64,
    counts: &HashMap<u64, u32>,
    path: &mut PathBuf,
    out: &mut HashMap<u64, Vec<PathBuf>>,
) {
    for c in &node.children {
        match c.kind {
            NodeKind::File => {
                if c.size >= min && counts.contains_key(&c.size) {
                    path.push(c.name.as_str());
                    out.entry(c.size).or_default().push(path.clone());
                    path.pop();
                }
            }
            NodeKind::Dir => {
                path.push(c.name.as_str());
                collect_paths(c, min, counts, path, out);
                path.pop();
            }
            _ => {}
        }
    }
}

/// Hash same-size files (head/tail first, then fully) and return the real duplicates.
pub fn hash_candidates(candidates: Vec<(u64, Vec<PathBuf>)>) -> Vec<DuplicateGroup> {
    let total: usize = candidates.iter().map(|(_, p)| p.len()).sum();
    if total < 2 {
        return Vec::new();
    }
    // One flat list of (size, path) in biggest-file-first order.
    let mut sizes: Vec<u64> = Vec::with_capacity(total);
    let mut paths: Vec<PathBuf> = Vec::with_capacity(total);
    for (size, group) in candidates {
        for p in group {
            sizes.push(size);
            paths.push(p);
        }
    }

    // Pass 2: quick hash. One small buffer per chunk, reused across the files in it.
    let per_chunk = chunk_size(paths.len());
    let keys: Vec<Option<Hash>> = paths
        .par_chunks(per_chunk)
        .enumerate()
        .map(|(ci, chunk)| {
            let start = ci * per_chunk;
            let mut buf = vec![0u8; 2 * PARTIAL as usize];
            chunk
                .iter()
                .enumerate()
                .map(|(i, p)| quick_key(p, sizes[start + i], &mut buf).ok())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>()
        .concat();

    let mut groups: Vec<DuplicateGroup> = Vec::new();
    let mut map: HashMap<(u64, Hash), Vec<u32>> = HashMap::with_capacity(paths.len() / 2);
    for (i, key) in keys.iter().enumerate() {
        if let Some(key) = key {
            map.entry((sizes[i], *key)).or_default().push(i as u32);
        }
    }
    // Groups that still need work: (size, candidate indices).
    let mut pending: Vec<(u64, Vec<u32>)> = Vec::new();
    for ((size, key), idxs) in map {
        if idxs.len() < 2 {
            continue;
        }
        if size <= SMALL {
            // The quick key covered the whole file.
            groups.push(make_group(size, key, &paths, &idxs));
        } else {
            pending.push((size, idxs));
        }
    }

    full_pass(&mut groups, pending, &sizes, &paths);

    groups.sort_unstable_by(|a, b| b.wasted.cmp(&a.wasted).then_with(|| a.paths.cmp(&b.paths)));
    groups
}

/// One unit of the full pass.
enum Task {
    /// Byte-compare the two copies of a group of exactly two files.
    Pair { group: u32, a: u32, b: u32 },
    /// Fully hash one file of a group of three or more.
    Hash { group: u32, cand: u32 },
}

#[derive(Clone)]
struct Hashed {
    cand: u32,
    hash: Hash,
}

/// Hash (or compare) every candidate that survived the quick pass, then build groups.
fn full_pass(
    groups: &mut Vec<DuplicateGroup>,
    mut pending: Vec<(u64, Vec<u32>)>,
    sizes: &[u64],
    paths: &[PathBuf],
) {
    if pending.is_empty() {
        return;
    }
    // Longest work first, so the pool is not left waiting on one huge group.
    pending.sort_unstable_by(|a, b| b.0.cmp(&a.0));

    let mut tasks: Vec<Task> = Vec::new();
    for (gi, (_, idxs)) in pending.iter().enumerate() {
        let gi = gi as u32;
        if let [a, b] = idxs[..] {
            tasks.push(Task::Pair { group: gi, a, b });
        } else {
            tasks.extend(idxs.iter().map(|&cand| Task::Hash { group: gi, cand }));
        }
    }
    tasks.sort_unstable_by_key(|t| {
        let size = match *t {
            Task::Pair { a, .. } => sizes[a as usize] * 2,
            Task::Hash { cand, .. } => sizes[cand as usize],
        };
        std::cmp::Reverse(size)
    });

    let per_chunk = chunk_size(tasks.len());
    let results: Vec<Option<Hash>> = tasks
        .par_chunks(per_chunk)
        .map(|chunk| {
            let mut buf = vec![0u8; BUF];
            // Only groups of two compare, so the second buffer is allocated on demand.
            let mut other: Option<Vec<u8>> = None;
            chunk
                .iter()
                .map(|t| match *t {
                    Task::Hash { cand, .. } => hash_full(&paths[cand as usize], &mut buf).ok(),
                    Task::Pair { a, b, .. } => {
                        let other = other.get_or_insert_with(|| vec![0u8; BUF]);
                        compare(&paths[a as usize], &paths[b as usize], &mut buf, other)
                            .unwrap_or(None)
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>()
        .concat();

    let mut compared: Vec<Option<Hash>> = vec![None; pending.len()];
    let mut hashed: Vec<Option<Vec<Hashed>>> = vec![None; pending.len()];
    for (task, result) in tasks.iter().zip(&results) {
        let Some(hash) = *result else { continue };
        match *task {
            Task::Pair { group, .. } => compared[group as usize] = Some(hash),
            Task::Hash { group, cand } => hashed[group as usize]
                .get_or_insert_with(Vec::new)
                .push(Hashed { cand, hash }),
        }
    }

    for (gi, (size, idxs)) in pending.iter().enumerate() {
        if let Some(hash) = compared[gi] {
            groups.push(make_group(*size, hash, paths, idxs));
            continue;
        }
        let Some(found) = hashed[gi].take() else {
            continue;
        };
        let mut by_hash: HashMap<Hash, Vec<u32>> = HashMap::new();
        for h in found {
            by_hash.entry(h.hash).or_default().push(h.cand);
        }
        for (hash, idxs) in by_hash {
            if idxs.len() > 1 {
                groups.push(make_group(*size, hash, paths, &idxs));
            }
        }
    }
}

fn make_group(size: u64, hash: Hash, paths: &[PathBuf], idxs: &[u32]) -> DuplicateGroup {
    let mut copies: Vec<PathBuf> = idxs.iter().map(|&i| paths[i as usize].clone()).collect();
    copies.sort();
    DuplicateGroup {
        size,
        wasted: size * (copies.len() as u64 - 1),
        hash: hash.to_hex().to_string(),
        paths: copies,
    }
}

/// Two chunks per thread are enough for load balancing while keeping the per-chunk
/// read buffers small; longest work is scheduled first.
fn chunk_size(len: usize) -> usize {
    let threads = rayon::current_num_threads().max(1);
    len.div_ceil(threads * 2).max(1)
}

/// Quick key of one file: the first and last block, or the whole content for files
/// small enough for that to cover everything.
fn quick_key(path: &Path, size: u64, buf: &mut [u8]) -> io::Result<Hash> {
    let mut f = File::open(path)?;
    let mut h = blake3::Hasher::new();
    if size <= SMALL {
        // At most 2 * PARTIAL bytes is the whole file.
        let n = fill(&mut f, &mut buf[..2 * PARTIAL as usize])?;
        h.update(&buf[..n]);
        return Ok(h.finalize());
    }
    let n = fill(&mut f, &mut buf[..PARTIAL as usize])?;
    h.update(&buf[..n]);
    // Bigger than SMALL, so the middle is guaranteed to exist: head + last block.
    f.seek(SeekFrom::End(-(PARTIAL as i64)))?;
    let n = fill(&mut f, &mut buf[..PARTIAL as usize])?;
    h.update(&buf[..n]);
    Ok(h.finalize())
}

fn hash_full(path: &Path, buf: &mut [u8]) -> io::Result<Hash> {
    let mut f = File::open(path)?;
    let mut h = blake3::Hasher::new();
    read_into(&mut f, &mut h, buf)?;
    Ok(h.finalize())
}

/// Hash `f` to the end using `buf` for reads.
fn read_into(f: &mut File, h: &mut blake3::Hasher, buf: &mut [u8]) -> io::Result<()> {
    loop {
        match f.read(buf)? {
            0 => return Ok(()),
            n => {
                h.update(&buf[..n]);
            }
        }
    }
}

/// Read `buf` completely (until it is full or the file ends). Returns the bytes read.
fn fill(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        match f.read(&mut buf[total..])? {
            0 => break,
            n => total += n,
        }
    }
    Ok(total)
}

/// Compare two files byte by byte, hashing `a` on the way.
///
/// Returns `Ok(None)` at the first difference (or at a length mismatch), which is
/// usually cheaper than hashing both files completely. `Ok(Some(hash))` means the
/// contents are identical and `hash` is the BLAKE3 hash of them.
fn compare(a: &Path, b: &Path, buf: &mut [u8], other: &mut [u8]) -> io::Result<Option<Hash>> {
    let mut fa = File::open(a)?;
    let mut fb = File::open(b)?;
    let mut h = blake3::Hasher::new();
    loop {
        let na = fill(&mut fa, buf)?;
        let nb = fill(&mut fb, other)?;
        if na != nb {
            return Ok(None);
        }
        if na == 0 {
            return Ok(Some(h.finalize()));
        }
        h.update(&buf[..na]);
        if buf[..na] != other[..nb] {
            return Ok(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{ScanOptions, scan};
    use std::fs;

    fn group_hashes(group: &DuplicateGroup) -> Vec<String> {
        let mut v: Vec<String> = group.paths.iter().map(|p| p.display().to_string()).collect();
        v.sort();
        v
    }

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
        // Small duplicates (whole file covered by the quick hash).
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

    #[test]
    fn same_size_different_tail_is_not_a_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let body: Vec<u8> = (0..300_000u32).map(|i| (i % 127) as u8).collect();
        let mut tail = body.clone();
        *tail.last_mut().unwrap() ^= 0xFF;
        fs::write(p.join("a"), &body).unwrap();
        fs::write(p.join("b"), &body).unwrap();
        fs::write(p.join("c"), &tail).unwrap();

        let s = scan(p, &ScanOptions::default()).unwrap();
        let g = find_duplicates(&s.tree, &s.root, &DupeOptions { min_size: 1 });
        assert_eq!(g.len(), 1);
        let names = group_hashes(&g[0]);
        assert!(names[0].ends_with('a') && names[1].ends_with('b'), "{names:?}");
    }

    #[test]
    fn identical_pairs_report_the_full_content_hash() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        // One file below SMALL, one above: both must carry the hash of all bytes.
        for (name, len) in [("small", 20_000usize), ("large", 400_000)] {
            let body: Vec<u8> = (0..len as u32).map(|i| (i % 241) as u8).collect();
            fs::write(p.join(format!("{name}1")), &body).unwrap();
            fs::write(p.join(format!("{name}2")), &body).unwrap();
            let s = scan(p, &ScanOptions::default()).unwrap();
            let g = find_duplicates(&s.tree, &s.root, &DupeOptions { min_size: 1 });
            let want = blake3::hash(&body).to_hex().to_string();
            let group = g
                .iter()
                .find(|g| g.size == len as u64)
                .unwrap_or_else(|| panic!("no group of {len} bytes"));
            assert_eq!(group.hash, want, "{name}");
            assert_eq!(group.wasted, len as u64);
        }
    }

    #[test]
    fn pair_differing_in_the_middle_is_not_a_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        // Bigger than SMALL, so the pair goes through byte comparison; the only
        // difference sits past the head block and before the tail block.
        let mut body: Vec<u8> = (0..2_000_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(p.join("a"), &body).unwrap();
        body[1_000_000] ^= 0xFF;
        fs::write(p.join("b"), &body).unwrap();

        let s = scan(p, &ScanOptions::default()).unwrap();
        let g = find_duplicates(&s.tree, &s.root, &DupeOptions { min_size: 1 });
        assert!(g.is_empty(), "{g:?}");
    }

    #[test]
    fn groups_of_three_split_by_content() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let body: Vec<u8> = (0..500_000u32).map(|i| (i % 233) as u8).collect();
        let mut other = body.clone();
        other[250_000] ^= 0xFF;
        fs::write(p.join("a"), &body).unwrap();
        fs::write(p.join("b"), &body).unwrap();
        fs::write(p.join("c"), &other).unwrap();

        let s = scan(p, &ScanOptions::default()).unwrap();
        let g = find_duplicates(&s.tree, &s.root, &DupeOptions { min_size: 1 });
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].paths.len(), 2);
        assert!(g[0].paths.iter().all(|p| !p.ends_with("c")));
    }

    #[test]
    fn unreadable_files_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let body: Vec<u8> = vec![7u8; 300_000];
        fs::write(p.join("a"), &body).unwrap();
        fs::write(p.join("b"), &body).unwrap();
        let missing = p.join("gone");
        let a = p.join("a");
        let b = p.join("b");

        // A missing file never forms a group, alone or next to a real copy.
        let g = hash_candidates(vec![(300_000, vec![missing.clone(), missing.clone()])]);
        assert!(g.is_empty(), "{g:?}");
        let g = hash_candidates(vec![(300_000, vec![a, b, missing])]);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].paths.len(), 2);
    }

    #[test]
    fn hard_links_count_as_identical_content() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let body: Vec<u8> = vec![3u8; 400_000];
        fs::write(p.join("a"), &body).unwrap();
        fs::hard_link(p.join("a"), p.join("link")).unwrap();

        let g = hash_candidates(vec![(400_000, vec![p.join("a"), p.join("link")])]);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].paths.len(), 2);
        assert_eq!(g[0].wasted, 400_000);
    }

    #[test]
    fn unique_sizes_and_min_size_are_filtered() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::write(p.join("only"), vec![1u8; 5000]).unwrap();
        fs::write(p.join("dup1"), vec![2u8; 100]).unwrap();
        fs::write(p.join("dup2"), vec![2u8; 100]).unwrap();

        let s = scan(p, &ScanOptions::default()).unwrap();
        let sizes = same_size_files(&s.tree, &s.root, &DupeOptions { min_size: 1 });
        assert_eq!(sizes.len(), 1, "{sizes:?}");
        assert_eq!(sizes[0].0, 100);
        assert_eq!(sizes[0].1.len(), 2);

        // Above the duplicate size everything is filtered out.
        let sizes = same_size_files(&s.tree, &s.root, &DupeOptions { min_size: 1000 });
        assert!(sizes.is_empty(), "{sizes:?}");
    }

    #[test]
    fn groups_are_sorted_by_wasted_space() {
        let waste_small: Vec<u8> = vec![9u8; 40_000];
        let waste_big: Vec<u8> = vec![8u8; 900_000];
        let candidates = vec![
            (40_000, vec![PathBuf::from("x1"), PathBuf::from("x2")]),
            (900_000, vec![PathBuf::from("y1"), PathBuf::from("y2")]),
        ];
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in [
            ("x1", &waste_small),
            ("x2", &waste_small),
            ("y1", &waste_big),
            ("y2", &waste_big),
        ] {
            fs::write(dir.path().join(name), body).unwrap();
        }
        let real = candidates
            .into_iter()
            .map(|(size, names)| {
                (
                    size,
                    names
                        .into_iter()
                        .map(|n| dir.path().join(n))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        let g = hash_candidates(real);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].size, 900_000);
        assert_eq!(g[0].wasted, 900_000);
        assert_eq!(g[1].size, 40_000);
    }

    #[test]
    fn single_candidate_is_not_a_group() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        fs::write(&a, b"data").unwrap();
        assert!(hash_candidates(vec![(4, vec![a])]).is_empty());
        assert!(hash_candidates(Vec::new()).is_empty());
    }
}
