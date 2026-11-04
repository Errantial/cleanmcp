use std::path::{Path, PathBuf};

use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Dir,
    /// Symlink or Windows junction. Recorded, never followed.
    Link,
    /// Sockets, devices, FIFOs.
    Other,
}

/// Which size to report and sort by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SizeMode {
    /// Space allocated on disk (what you get back by deleting). Default.
    #[default]
    Disk,
    /// Logical file length ("apparent size").
    Apparent,
}

/// Marker for "modification time unknown".
pub const NO_TIME: i64 = i64::MIN;

/// One file or directory in a scanned tree. Kept small: a big drive has millions of these.
///
/// For directories, sizes and counts include everything below them.
#[derive(Debug, Clone)]
pub struct Node {
    pub name: CompactString,
    /// Logical size in bytes.
    pub size: u64,
    /// Allocated size on disk in bytes.
    pub disk_size: u64,
    /// Modification time as Unix seconds, or [`NO_TIME`].
    pub mtime: i64,
    /// Files below this node (1 for a file).
    pub file_count: u32,
    /// Directories below this node (not counting itself).
    pub dir_count: u32,
    /// Entries below this node that could not be read (permission denied, etc.).
    pub errors: u32,
    pub kind: NodeKind,
    /// Children sorted by disk size, largest first. Empty for non-directories.
    pub children: Box<[Node]>,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        self.kind == NodeKind::Dir
    }

    pub fn size_by(&self, mode: SizeMode) -> u64 {
        match mode {
            SizeMode::Disk => self.disk_size,
            SizeMode::Apparent => self.size,
        }
    }

    pub fn modified(&self) -> Option<SystemTime> {
        if self.mtime == NO_TIME {
            None
        } else if self.mtime >= 0 {
            Some(UNIX_EPOCH + Duration::from_secs(self.mtime as u64))
        } else {
            UNIX_EPOCH.checked_sub(Duration::from_secs(self.mtime.unsigned_abs()))
        }
    }

    /// Time since last modification, if known and in the past.
    pub fn age(&self) -> Option<Duration> {
        SystemTime::now().duration_since(self.modified()?).ok()
    }

    /// Children sorted by `mode`, largest first.
    pub fn sorted_children(&self, mode: SizeMode) -> Vec<&Node> {
        let mut v: Vec<&Node> = self.children.iter().collect();
        if mode != SizeMode::Disk {
            v.sort_by_key(|c| std::cmp::Reverse(c.size_by(mode)));
        }
        v
    }

    /// Find a descendant by path relative to this node, e.g. `"src/main.rs"`.
    pub fn find(&self, rel: &Path) -> Option<&Node> {
        let mut node = self;
        for part in rel.components() {
            let name = part.as_os_str().to_string_lossy();
            if name == "." {
                continue;
            }
            node = node.children.iter().find(|c| *c.name == *name)?;
        }
        Some(node)
    }

    /// Depth-first walk over every node, giving each node's full path.
    pub fn walk<'a>(&'a self, base: &Path, f: &mut impl FnMut(&Path, &'a Node)) {
        f(base, self);
        for child in &self.children {
            child.walk(&base.join(child.name.as_str()), f);
        }
    }

    /// Recompute this directory's totals from its children and re-sort them.
    pub(crate) fn finish_dir(&mut self) {
        let (mut size, mut disk, mut files, mut dirs, mut errors) =
            (0u64, 0u64, 0u32, 0u32, self.errors);
        for c in &self.children {
            size += c.size;
            disk += c.disk_size;
            files = files.saturating_add(c.file_count);
            errors = errors.saturating_add(c.errors);
            if c.is_dir() {
                dirs = dirs.saturating_add(1).saturating_add(c.dir_count);
            }
        }
        self.size = size;
        self.disk_size = disk;
        self.file_count = files;
        self.dir_count = dirs;
        self.errors = errors;
        self.children
            .sort_unstable_by_key(|c| std::cmp::Reverse(c.disk_size));
    }
}

/// Counters collected while scanning.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanStats {
    /// Extra hard links to an already-counted file (counted as 0 bytes).
    pub hardlinks_deduped: u64,
    /// Directories skipped because they are on another filesystem.
    pub skipped_mounts: u64,
    /// Entries skipped by `--exclude` or the virtual-filesystem filter.
    pub excluded: u64,
    /// The scan was cancelled; totals are partial.
    pub cancelled: bool,
}

/// The options a scan was made with, kept so it can be repeated the same way
/// (e.g. by `diff` when rescanning).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanSettings {
    pub one_filesystem: bool,
    pub include_virtual: bool,
    pub dedupe_hardlinks: bool,
    pub exclude: Vec<String>,
}

/// A scanned tree plus the root path it was scanned from.
#[derive(Debug, Clone)]
pub struct Scan {
    pub root: PathBuf,
    pub tree: Node,
    /// Unix seconds when the scan finished.
    pub scanned_at: i64,
    pub elapsed_ms: u64,
    pub stats: ScanStats,
    pub settings: ScanSettings,
}

impl Scan {
    /// Look up a node by absolute path or path relative to the scan root.
    pub fn find(&self, path: &Path) -> Option<&Node> {
        self.tree.find(self.relative(path)?)
    }

    fn relative<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        if path.is_absolute() {
            path.strip_prefix(&self.root).ok()
        } else {
            Some(path)
        }
    }

    /// Remove the node at `path` (e.g. after deleting it) and subtract it from every ancestor.
    /// Returns the removed node. The root itself cannot be removed.
    pub fn remove(&mut self, path: &Path) -> Option<Node> {
        let rel = self.relative(path)?.to_path_buf();
        let names: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .filter(|n| n != ".")
            .collect();
        if names.is_empty() {
            return None;
        }
        remove_rec(&mut self.tree, &names)
    }
}

fn remove_rec(node: &mut Node, names: &[String]) -> Option<Node> {
    let idx = node.children.iter().position(|c| *c.name == *names[0])?;
    let removed = if names.len() == 1 {
        let mut v = std::mem::take(&mut node.children).into_vec();
        let r = v.remove(idx);
        node.children = v.into_boxed_slice();
        r
    } else {
        remove_rec(&mut node.children[idx], &names[1..])?
    };
    node.size -= removed.size;
    node.disk_size -= removed.disk_size;
    node.file_count = node.file_count.saturating_sub(removed.file_count);
    node.errors = node.errors.saturating_sub(removed.errors);
    node.dir_count = node
        .dir_count
        .saturating_sub(removed.dir_count + removed.is_dir() as u32);
    node.children
        .sort_unstable_by_key(|c| std::cmp::Reverse(c.disk_size));
    Some(removed)
}
