//! Read-only questions about a scanned tree.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::tree::{Node, NodeKind, SizeMode};

const DAY: u64 = 86_400;

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Entry {
    pub path: PathBuf,
    /// Allocated size on disk in bytes.
    pub disk_size: u64,
    /// Logical (apparent) size in bytes.
    pub size: u64,
    pub is_dir: bool,
    pub file_count: u64,
    /// Whole days since last modification, if known.
    pub age_days: Option<u64>,
}

impl Entry {
    pub fn from_node(path: &Path, n: &Node) -> Self {
        Entry {
            path: path.to_path_buf(),
            disk_size: n.disk_size,
            size: n.size,
            is_dir: n.is_dir(),
            file_count: n.file_count() as u64,
            age_days: n.age().map(|d| d.as_secs() / DAY),
        }
    }

    pub fn size_by(&self, mode: SizeMode) -> u64 {
        match mode {
            SizeMode::Disk => self.disk_size,
            SizeMode::Apparent => self.size,
        }
    }
}

/// Which files a query should include. The default filter accepts every file.
#[derive(Debug, Clone, Default)]
pub struct FileFilter {
    /// Minimum size in bytes (measured with the query's [`SizeMode`]).
    pub min_size: Option<u64>,
    /// Only files not modified for at least this many days.
    pub older_than_days: Option<u64>,
    /// Only files modified less than this many days ago.
    pub newer_than_days: Option<u64>,
    /// Only these extensions (case-insensitive, no dot). Empty = all.
    pub extensions: Vec<String>,
}

impl FileFilter {
    pub fn matches(&self, node: &Node, mode: SizeMode) -> bool {
        if node.kind() != NodeKind::File {
            return false;
        }
        if self.min_size.is_some_and(|m| node.size_by(mode) < m) {
            return false;
        }
        // A modification time in the future counts as age 0 (brand new).
        let age_days = node.modified().map(|t| {
            std::time::SystemTime::now()
                .duration_since(t)
                .map_or(0, |a| a.as_secs() / DAY)
        });
        if let Some(min) = self.older_than_days
            && age_days.is_none_or(|a| a < min)
        {
            return false;
        }
        if let Some(max) = self.newer_than_days
            && age_days.is_none_or(|a| a >= max)
        {
            return false;
        }
        if !self.extensions.is_empty() {
            let ext = extension(&node.name);
            if !self
                .extensions
                .iter()
                .any(|e| e.trim_start_matches('.').eq_ignore_ascii_case(&ext))
            {
                return false;
            }
        }
        true
    }
}

/// Lowercased extension without the dot, or "(none)".
pub fn extension(name: &str) -> String {
    Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| "(none)".into())
}

/// The `n` largest files anywhere below `base` that pass `filter`.
pub fn largest_files(
    root: &Node,
    base: &Path,
    n: usize,
    filter: &FileFilter,
    mode: SizeMode,
) -> Vec<Entry> {
    if n == 0 {
        return Vec::new();
    }
    // Bounded min-heap over slots, so memory stays O(n) on huge trees.
    let mut slots: Vec<(PathBuf, &Node)> = Vec::with_capacity(n);
    let mut heap: BinaryHeap<Reverse<(u64, usize)>> = BinaryHeap::with_capacity(n);
    root.walk(base, &mut |p, node| {
        if !filter.matches(node, mode) {
            return;
        }
        let size = node.size_by(mode);
        if slots.len() < n {
            heap.push(Reverse((size, slots.len())));
            slots.push((p.to_path_buf(), node));
        } else if let Some(&Reverse((min, idx))) = heap.peek()
            && size > min
        {
            heap.pop();
            slots[idx] = (p.to_path_buf(), node);
            heap.push(Reverse((size, idx)));
        }
    });
    let mut out: Vec<Entry> = slots.iter().map(|(p, n)| Entry::from_node(p, n)).collect();
    out.sort_unstable_by_key(|e| Reverse(e.size_by(mode)));
    out
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ExtensionStat {
    /// Lowercased extension without the dot, or "(none)".
    pub extension: String,
    pub disk_size: u64,
    pub size: u64,
    pub file_count: u64,
}

impl ExtensionStat {
    pub fn size_by(&self, mode: SizeMode) -> u64 {
        match mode {
            SizeMode::Disk => self.disk_size,
            SizeMode::Apparent => self.size,
        }
    }
}

/// Total size per file extension, largest first.
pub fn by_extension(root: &Node, n: usize, mode: SizeMode) -> Vec<ExtensionStat> {
    let mut map: HashMap<String, ExtensionStat> = HashMap::new();
    root.walk(Path::new(""), &mut |_, node| {
        if node.kind() == NodeKind::File {
            let e = map
                .entry(extension(&node.name))
                .or_insert_with_key(|k| ExtensionStat {
                    extension: k.clone(),
                    disk_size: 0,
                    size: 0,
                    file_count: 0,
                });
            e.disk_size += node.disk_size;
            e.size += node.size;
            e.file_count += 1;
        }
    });
    let mut out: Vec<_> = map.into_values().collect();
    out.sort_unstable_by_key(|s| Reverse(s.size_by(mode)));
    out.truncate(n);
    out
}

/// Age buckets used by [`by_age`]: (label, exclusive upper bound in days).
pub const AGE_BUCKETS: &[(&str, u64)] = &[
    ("< 1 month", 30),
    ("1-3 months", 91),
    ("3-6 months", 182),
    ("6-12 months", 365),
    ("1-2 years", 730),
    ("2-5 years", 1826),
    ("> 5 years", u64::MAX),
];

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct AgeStat {
    /// e.g. "1-3 months" since last modification; "unknown" if there is no usable timestamp.
    pub bucket: String,
    pub disk_size: u64,
    pub size: u64,
    pub file_count: u64,
}

/// How much data was last modified in each age bucket (TreeSize's "age of files").
pub fn by_age(root: &Node) -> Vec<AgeStat> {
    let mut stats: Vec<AgeStat> = AGE_BUCKETS
        .iter()
        .map(|(l, _)| *l)
        .chain(std::iter::once("unknown"))
        .map(|bucket| AgeStat {
            bucket: bucket.into(),
            disk_size: 0,
            size: 0,
            file_count: 0,
        })
        .collect();
    root.walk(Path::new(""), &mut |_, node| {
        if node.kind() != NodeKind::File {
            return;
        }
        let idx = match node.age() {
            Some(a) => {
                let days = a.as_secs() / DAY;
                AGE_BUCKETS
                    .iter()
                    .position(|(_, max)| days < *max)
                    .unwrap_or(AGE_BUCKETS.len() - 1)
            }
            // No timestamp, or modified in the future.
            None => AGE_BUCKETS.len(),
        };
        let s = &mut stats[idx];
        s.disk_size += node.disk_size;
        s.size += node.size;
        s.file_count += 1;
    });
    stats
}

/// Format Unix seconds as `YYYY-MM-DD HH:MM:SS` (UTC).
pub fn format_time(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{ScanOptions, scan};
    use std::fs;

    #[test]
    fn format_time_known_values() {
        assert_eq!(format_time(0), "1970-01-01 00:00:00");
        assert_eq!(format_time(951_782_400), "2000-02-29 00:00:00");
        assert_eq!(format_time(1_700_000_000), "2023-11-14 22:13:20");
    }

    #[test]
    fn filters_and_top_n() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::write(p.join("a.iso"), vec![1u8; 30_000]).unwrap();
        fs::write(p.join("b.iso"), vec![1u8; 20_000]).unwrap();
        fs::write(p.join("c.txt"), vec![1u8; 50_000]).unwrap();
        fs::write(p.join("d.txt"), vec![1u8; 10]).unwrap();
        let s = scan(p, &ScanOptions::default()).unwrap();
        let m = SizeMode::Apparent;
        let top = |n, f: &FileFilter| largest_files(&s.tree, &s.root, n, f, m);

        let all = top(2, &FileFilter::default());
        assert_eq!(all.len(), 2);
        assert!(all[0].path.ends_with("c.txt"));
        assert!(all[1].path.ends_with("a.iso"));

        let iso = FileFilter {
            extensions: vec![".ISO".into()],
            ..Default::default()
        };
        assert_eq!(top(10, &iso).len(), 2);

        let big = FileFilter {
            min_size: Some(25_000),
            ..Default::default()
        };
        assert_eq!(top(10, &big).len(), 2);

        // Just-written files are 0 days old.
        let old = FileFilter {
            older_than_days: Some(1),
            ..Default::default()
        };
        assert!(top(10, &old).is_empty());
        let new = FileFilter {
            newer_than_days: Some(1),
            ..Default::default()
        };
        assert_eq!(top(10, &new).len(), 4);

        assert_eq!(by_age(&s.tree)[0].file_count, 4);
    }

    #[test]
    fn future_mtime_is_new() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("future");
        fs::write(&f, b"x").unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(86_400 * 30);
        fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(later)
            .unwrap();
        let s = scan(dir.path(), &ScanOptions::default()).unwrap();
        let new = FileFilter {
            newer_than_days: Some(1),
            ..Default::default()
        };
        assert_eq!(
            largest_files(&s.tree, &s.root, 10, &new, SizeMode::Disk).len(),
            1
        );
    }
}
