//! Read-only questions about a scanned tree.
//!
//! Every query walks the whole tree, so the per-node cost matters: these
//! functions never build a `PathBuf` (or any other allocation) per node. Paths
//! are materialised only for the rows that end up in the result.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::tree::{NO_TIME, Node, NodeKind, SizeMode};

const DAY: u64 = 86_400;

/// Extension shown for files without one.
const NO_EXT: &str = "(none)";

/// Longest extension handled without allocating.
const EXT_BUF: usize = 32;

/// Longest name lowercased without allocating.
pub(crate) const NAME_BUF: usize = 48;

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
        Matcher::new(self).matches(node, mode)
    }
}

/// A [`FileFilter`] prepared for a whole walk: the clock is read once and the
/// extension list is lowercased once instead of once per file.
struct Matcher<'a> {
    filter: &'a FileFilter,
    /// Unix seconds when the query started; `None` time compares as 0 days old.
    now: i64,
    /// `filter.extensions` lowercased, without the leading dot.
    exts: Vec<String>,
}

impl<'a> Matcher<'a> {
    fn new(filter: &'a FileFilter) -> Self {
        Matcher {
            filter,
            now: unix_now(),
            exts: filter
                .extensions
                .iter()
                .map(|e| e.trim_start_matches('.').to_lowercase())
                .collect(),
        }
    }

    fn matches(&self, node: &Node, mode: SizeMode) -> bool {
        if node.kind() != NodeKind::File {
            return false;
        }
        if self.filter.min_size.is_some_and(|m| node.size_by(mode) < m) {
            return false;
        }
        if self.filter.older_than_days.is_some() || self.filter.newer_than_days.is_some() {
            // A modification time in the future counts as age 0 (brand new);
            // an unknown one matches neither age filter.
            let Some(age_days) = node_age_days(node, self.now) else {
                return false;
            };
            if let Some(min) = self.filter.older_than_days
                && age_days < min
            {
                return false;
            }
            if let Some(max) = self.filter.newer_than_days
                && age_days >= max
            {
                return false;
            }
        }
        if !self.exts.is_empty() {
            let mut buf = [0u8; EXT_BUF];
            let name = node.name.as_str();
            let hit = match lower_ext(name, &mut buf) {
                Some(ext) => self.exts.iter().any(|e| e == ext),
                // Long or non-ASCII extension: rare, fall back to allocating.
                None => {
                    let ext = extension(name);
                    self.exts.contains(&ext)
                }
            };
            if !hit {
                return false;
            }
        }
        true
    }
}

/// Whole days since `node` was last modified, as of `now` (Unix seconds).
/// `None` when the timestamp is unknown; a future timestamp counts as 0.
fn node_age_days(node: &Node, now: i64) -> Option<u64> {
    if node.mtime == NO_TIME {
        return None;
    }
    let secs = now.saturating_sub(node.mtime as i64);
    Some(if secs <= 0 { 0 } else { secs as u64 / DAY })
}

pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The extension of `name` as `std::path::Path::extension` defines it.
fn ext_of(name: &str) -> Option<&str> {
    if name == "." || name == ".." {
        return None;
    }
    let bytes = name.as_bytes();
    // Only the last path component counts; a separator means "no extension".
    let start = match bytes.iter().rposition(|b| *b == b'/' || *b == b'\\') {
        Some(i) => i + 1,
        None => 0,
    };
    let dot = start + bytes[start..].iter().rposition(|b| *b == b'.')?;
    // A leading dot (".gitignore") is a hidden file, not an extension.
    if dot == start {
        return None;
    }
    Some(&name[dot + 1..])
}

/// Lowercased extension of `name` written into `buf` (no allocation), or
/// `"(none)"`. `None` means "too long or not ASCII": use [`extension`].
fn lower_ext<'b>(name: &str, buf: &'b mut [u8; EXT_BUF]) -> Option<&'b str> {
    let Some(ext) = ext_of(name) else {
        return Some(NO_EXT);
    };
    if ext.len() > EXT_BUF || !ext.is_ascii() {
        return None;
    }
    let out = &mut buf[..ext.len()];
    out.copy_from_slice(ext.as_bytes());
    out.make_ascii_lowercase();
    std::str::from_utf8(out).ok()
}

/// Lowercased extension without the dot, or "(none)".
pub fn extension(name: &str) -> String {
    ext_of(name)
        .map(|e| e.to_lowercase())
        .unwrap_or_else(|| NO_EXT.into())
}

/// ASCII-lowercase `s` into `buf` without allocating. `None` when it does not
/// fit or is not ASCII (then the caller keeps its allocating path).
pub(crate) fn lower_into<'b>(s: &str, buf: &'b mut [u8; NAME_BUF]) -> Option<&'b str> {
    if s.len() > NAME_BUF || !s.is_ascii() {
        return None;
    }
    let out = &mut buf[..s.len()];
    out.copy_from_slice(s.as_bytes());
    out.make_ascii_lowercase();
    std::str::from_utf8(out).ok()
}

/// Visit every file node, without building paths.
fn visit_files(node: &Node, f: &mut impl FnMut(&Node)) {
    if node.kind() == NodeKind::File {
        f(node);
        return;
    }
    for c in node.children().iter() {
        visit_files(c, f);
    }
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
    let matcher = Matcher::new(filter);
    let mut top = TopN::new(n);
    let mut path = base.to_path_buf();
    collect_largest(root, &mut path, &matcher, mode, &mut top);
    top.into_entries(mode)
}

fn collect_largest<'a>(
    node: &'a Node,
    path: &mut PathBuf,
    matcher: &Matcher<'_>,
    mode: SizeMode,
    top: &mut TopN<'a>,
) {
    for c in node.children().iter() {
        match c.kind() {
            // Files are the only matches, so they never need a path of their own.
            NodeKind::File => {
                if matcher.matches(c, mode) {
                    top.offer(path, c, mode);
                }
            }
            _ => {
                path.push(c.name.as_str());
                collect_largest(c, path, matcher, mode, top);
                path.pop();
            }
        }
    }
}

/// Bounded min-heap over slots, so memory stays O(n) on huge trees.
struct TopN<'a> {
    slots: Vec<(PathBuf, &'a Node)>,
    heap: BinaryHeap<Reverse<(u64, usize)>>,
    cap: usize,
}

impl<'a> TopN<'a> {
    fn new(cap: usize) -> Self {
        TopN {
            slots: Vec::with_capacity(cap),
            heap: BinaryHeap::with_capacity(cap),
            cap,
        }
    }

    fn offer(&mut self, parent: &Path, node: &'a Node, mode: SizeMode) {
        let size = node.size_by(mode);
        if self.slots.len() < self.cap {
            self.heap.push(Reverse((size, self.slots.len())));
            self.slots.push((parent.join(node.name.as_str()), node));
        } else if let Some(&Reverse((min, idx))) = self.heap.peek()
            && size > min
        {
            self.heap.pop();
            self.slots[idx] = (parent.join(node.name.as_str()), node);
            self.heap.push(Reverse((size, idx)));
        }
    }

    fn into_entries(self, mode: SizeMode) -> Vec<Entry> {
        let mut out: Vec<Entry> = self
            .slots
            .iter()
            .map(|(p, n)| Entry::from_node(p, n))
            .collect();
        out.sort_unstable_by_key(|e| Reverse(e.size_by(mode)));
        out
    }
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
    let mut stats: Vec<ExtensionStat> = Vec::new();
    // Extension -> index into `stats`. Looked up with a borrowed &str so the
    // common case (an extension we have already seen) allocates nothing.
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut add = |buf_key: &str, node: &Node| match index.get(buf_key) {
        Some(&i) => {
            let s = &mut stats[i];
            s.disk_size += node.disk_size;
            s.size += node.size;
            s.file_count += 1;
        }
        None => {
            index.insert(buf_key.to_owned(), stats.len());
            stats.push(ExtensionStat {
                extension: buf_key.to_owned(),
                disk_size: node.disk_size,
                size: node.size,
                file_count: 1,
            });
        }
    };
    let mut buf = [0u8; EXT_BUF];
    visit_files(root, &mut |node| {
        let name = node.name.as_str();
        match lower_ext(name, &mut buf) {
            Some(ext) => add(ext, node),
            None => add(&extension(name), node),
        }
    });
    let mut out = stats;
    // Sorted by size; ties by extension so the output is stable between runs.
    out.sort_unstable_by(|a, b| {
        b.size_by(mode)
            .cmp(&a.size_by(mode))
            .then_with(|| a.extension.cmp(&b.extension))
    });
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
    let now = unix_now();
    visit_files(root, &mut |node| {
        let idx = match node_age_days(node, now) {
            Some(days) => AGE_BUCKETS
                .iter()
                .position(|(_, max)| days < *max)
                .unwrap_or(AGE_BUCKETS.len() - 1),
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
    fn fast_extension_matches_std() {
        for name in [
            "a.txt",
            "a",
            ".gitignore",
            "..",
            ".",
            "a.",
            "a.b.c",
            ".a.b",
            "a/b.txt",
            "a.b/c",
            "(none)",
            "x.(none)",
            "UPPER.TXT",
            "näme.äö",
            "trailing.",
            "....",
            "a..b",
        ] {
            let std_ext = Path::new(name)
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_else(|| NO_EXT.into());
            assert_eq!(extension(name), std_ext, "name {name:?}");
            let mut buf = [0u8; EXT_BUF];
            match lower_ext(name, &mut buf) {
                Some(fast) => assert_eq!(fast, std_ext, "name {name:?}"),
                None => assert!(std_ext.len() > EXT_BUF || !std_ext.is_ascii()),
            }
        }
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
    fn nested_paths_and_extension_totals() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::create_dir_all(p.join("a/b/c")).unwrap();
        fs::write(p.join("a/b/c/deep.TXT"), vec![1u8; 4_000]).unwrap();
        fs::write(p.join("a/one.txt"), vec![1u8; 3_000]).unwrap();
        fs::write(p.join("a/noext"), vec![1u8; 2_000]).unwrap();
        let s = scan(p, &ScanOptions::default()).unwrap();
        let m = SizeMode::Apparent;

        let top = largest_files(&s.tree, &s.root, 1, &FileFilter::default(), m);
        assert_eq!(top.len(), 1);
        assert_eq!(
            top[0].path,
            s.root.join("a").join("b").join("c").join("deep.TXT")
        );

        let stats = by_extension(&s.tree, 10, m);
        assert_eq!(stats[0].extension, "txt");
        assert_eq!(stats[0].file_count, 2);
        assert_eq!(stats[0].size, 7_000);
        assert_eq!(stats[1].extension, "(none)");
        assert_eq!(stats[1].file_count, 1);

        let nested = s.tree.find(Path::new("a")).unwrap();
        let from_sub = largest_files(nested, &s.root.join("a"), 10, &FileFilter::default(), m);
        assert_eq!(from_sub.len(), 3);
        assert!(from_sub[0].path.ends_with("deep.TXT"));
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

    #[test]
    fn age_filters_use_whole_days() {
        // A file exactly 10 days old is "10 days" for both filters.
        let now = unix_now();
        let node = |mtime: u32| {
            let mut n = Node::new(NodeKind::File, "f".into(), 1, 1, mtime);
            n.set_counts(1, 0, 0);
            n
        };
        let ten_days = now - 10 * DAY as i64;
        let f = FileFilter {
            older_than_days: Some(10),
            ..Default::default()
        };
        assert!(f.matches(&node(ten_days as u32), SizeMode::Disk));
        assert!(!f.matches(&node(ten_days as u32 + 1), SizeMode::Disk));
        let f = FileFilter {
            newer_than_days: Some(10),
            ..Default::default()
        };
        assert!(!f.matches(&node(ten_days as u32), SizeMode::Disk));
        assert!(f.matches(&node(ten_days as u32 + 1), SizeMode::Disk));
        // Unknown timestamps match neither filter.
        let unknown = node(NO_TIME);
        assert!(
            !FileFilter {
                older_than_days: Some(0),
                ..Default::default()
            }
            .matches(&unknown, SizeMode::Disk)
        );
    }
}
