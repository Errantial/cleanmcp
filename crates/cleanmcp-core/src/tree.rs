use std::path::{Path, PathBuf};
use std::ptr::NonNull;

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

/// Marker for "modification time unknown" in [`Node::mtime`].
pub const NO_TIME: u32 = u32::MAX;

/// Convert Unix seconds (from metadata or a snapshot file) to [`Node::mtime`]'s form.
///
/// Times before 1970 are clamped to the epoch (they are "very old" either way) and
/// `i64::MIN`, the snapshot sentinel for "no timestamp", becomes [`NO_TIME`].
pub fn mtime_to_u32(secs: i64) -> u32 {
    if secs == i64::MIN {
        NO_TIME
    } else {
        secs.clamp(0, u32::MAX as i64 - 1) as u32
    }
}

// `Node::meta` packs the counters and the kind into eight bytes:
// kind (2 bits) | errors (12 bits) | dir_count (22 bits) | file_count (28 bits).
// Counters saturate at these limits, which are far above any real subtree.
const KIND_BITS: u32 = 2;
const ERRORS_SHIFT: u32 = KIND_BITS;
const ERRORS_BITS: u32 = 12;
const DIRS_SHIFT: u32 = ERRORS_SHIFT + ERRORS_BITS;
const DIRS_BITS: u32 = 22;
const FILES_SHIFT: u32 = DIRS_SHIFT + DIRS_BITS;
const FILES_BITS: u32 = 28;

const ERRORS_MAX: u32 = (1 << ERRORS_BITS) - 1;
const DIRS_MAX: u32 = (1 << DIRS_BITS) - 1;
const FILES_MAX: u32 = (1 << FILES_BITS) - 1;

fn kind_bits(kind: NodeKind) -> u64 {
    match kind {
        NodeKind::File => 0,
        NodeKind::Dir => 1,
        NodeKind::Link => 2,
        NodeKind::Other => 3,
    }
}

fn kind_of_bits(bits: u64) -> NodeKind {
    match bits {
        0 => NodeKind::File,
        1 => NodeKind::Dir,
        2 => NodeKind::Link,
        _ => NodeKind::Other,
    }
}

/// One file or directory in a scanned tree. Kept small: a big drive has millions of these.
///
/// For directories, sizes and counts include everything below them. Children live in
/// one exact-size heap allocation, reached with [`Node::children`].
pub struct Node {
    pub name: CompactString,
    /// Logical size in bytes.
    pub size: u64,
    /// Allocated size on disk in bytes.
    pub disk_size: u64,
    /// Counters and kind, packed. Use [`Node::file_count`], [`Node::dir_count`],
    /// [`Node::errors`] and [`Node::kind`].
    meta: u64,
    /// First of `kids_len` children (sorted by disk size, largest first).
    kids: NonNull<Node>,
    kids_len: u32,
    /// Modification time as Unix seconds, or [`NO_TIME`]. See [`mtime_to_u32`].
    pub mtime: u32,
}

// SAFETY: a `Node` uniquely owns its children, and the data in it is `Send + Sync`
// (`CompactString` and plain integers), so moving or sharing a tree across threads is sound.
unsafe impl Send for Node {}
unsafe impl Sync for Node {}

impl Node {
    /// One node with no children and zero counts.
    pub fn new(kind: NodeKind, name: CompactString, size: u64, disk_size: u64, mtime: u32) -> Self {
        Node {
            name,
            size,
            disk_size,
            meta: kind_bits(kind),
            kids: NonNull::dangling(),
            kids_len: 0,
            mtime,
        }
    }

    pub fn kind(&self) -> NodeKind {
        kind_of_bits(self.meta & ((1 << KIND_BITS) - 1))
    }

    pub fn is_dir(&self) -> bool {
        self.kind() == NodeKind::Dir
    }

    pub fn is_file(&self) -> bool {
        self.kind() == NodeKind::File
    }

    /// Files below this node (1 for a file).
    pub fn file_count(&self) -> u32 {
        ((self.meta >> FILES_SHIFT) & FILES_MAX as u64) as u32
    }

    /// Directories below this node (not counting itself).
    pub fn dir_count(&self) -> u32 {
        ((self.meta >> DIRS_SHIFT) & DIRS_MAX as u64) as u32
    }

    /// Entries below this node that could not be read (permission denied, etc.).
    pub fn errors(&self) -> u32 {
        ((self.meta >> ERRORS_SHIFT) & ERRORS_MAX as u64) as u32
    }

    /// Set the counters, saturating at the packed limits.
    pub fn set_counts(&mut self, files: u32, dirs: u32, errors: u32) {
        self.meta = (self.meta & ((1 << KIND_BITS) - 1))
            | ((errors.min(ERRORS_MAX) as u64) << ERRORS_SHIFT)
            | ((dirs.min(DIRS_MAX) as u64) << DIRS_SHIFT)
            | ((files.min(FILES_MAX) as u64) << FILES_SHIFT);
    }

    /// Record one more entry that could not be read.
    pub fn add_error(&mut self) {
        self.set_counts(self.file_count(), self.dir_count(), self.errors() + 1);
    }

    /// The children, sorted by disk size (largest first). Empty for non-directories.
    pub fn children(&self) -> &[Node] {
        if self.kids_len == 0 {
            &[]
        } else {
            // SAFETY: a non-empty node owns exactly `kids_len` initialised children
            // in one allocation, and `&self` keeps them alive.
            unsafe { std::slice::from_raw_parts(self.kids.as_ptr(), self.kids_len as usize) }
        }
    }

    pub fn children_mut(&mut self) -> &mut [Node] {
        if self.kids_len == 0 {
            &mut []
        } else {
            // SAFETY: as `children`, and `&mut self` makes the borrow exclusive.
            unsafe { std::slice::from_raw_parts_mut(self.kids.as_ptr(), self.kids_len as usize) }
        }
    }

    /// Replace the children. The vector's allocation is reused when it is exactly sized.
    pub fn set_children(&mut self, children: Vec<Node>) {
        let (ptr, len) = kids_from_vec(children);
        let (old_ptr, old_len) = (self.kids, self.kids_len);
        self.kids = ptr;
        self.kids_len = len;
        // SAFETY: the old children were owned by this node and are no longer referenced.
        unsafe { free_kids(old_ptr, old_len) }
    }

    /// Take the children out as a `Vec` (exact capacity), leaving this node childless.
    pub fn take_children(&mut self) -> Vec<Node> {
        let len = self.kids_len as usize;
        let ptr = self.kids.as_ptr();
        self.kids = NonNull::dangling();
        self.kids_len = 0;
        if len == 0 {
            Vec::new()
        } else {
            // SAFETY: this node owned exactly `len` children in one allocation and has
            // just given up its pointer to them.
            unsafe { Vec::from_raw_parts(ptr, len, len) }
        }
    }

    /// Modification time as Unix seconds (negative before 1970, `i64::MIN` if unknown).
    pub fn mtime_unix(&self) -> i64 {
        if self.mtime == NO_TIME {
            i64::MIN
        } else {
            self.mtime as i64
        }
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
        } else {
            UNIX_EPOCH.checked_add(Duration::from_secs(self.mtime as u64))
        }
    }

    /// Time since last modification, if known and in the past.
    pub fn age(&self) -> Option<Duration> {
        SystemTime::now().duration_since(self.modified()?).ok()
    }

    /// Children sorted by `mode`, largest first.
    pub fn sorted_children(&self, mode: SizeMode) -> Vec<&Node> {
        let mut v: Vec<&Node> = self.children().iter().collect();
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
            node = node.children().iter().find(|c| *c.name == *name)?;
        }
        Some(node)
    }

    /// Depth-first walk over every node, giving each node's full path.
    pub fn walk<'a>(&'a self, base: &Path, f: &mut impl FnMut(&Path, &'a Node)) {
        f(base, self);
        for child in self.children() {
            child.walk(&base.join(child.name.as_str()), f);
        }
    }

    /// Recompute this directory's totals from its children and re-sort them.
    pub(crate) fn finish_dir(&mut self) {
        let (mut size, mut disk, mut files, mut dirs) = (0u64, 0u64, 0u32, 0u32);
        let mut errors = self.errors();
        for c in self.children() {
            size += c.size;
            disk += c.disk_size;
            files = files.saturating_add(c.file_count());
            errors = errors.saturating_add(c.errors());
            if c.is_dir() {
                dirs = dirs.saturating_add(1).saturating_add(c.dir_count());
            }
        }
        self.size = size;
        self.disk_size = disk;
        self.set_counts(files, dirs, errors);
        self.children_mut()
            .sort_unstable_by_key(|c| std::cmp::Reverse(c.disk_size));
    }
}

/// Move an exactly-sized vector of nodes to the heap and return its pointer and length.
fn kids_from_vec(v: Vec<Node>) -> (NonNull<Node>, u32) {
    assert!(
        v.len() <= u32::MAX as usize,
        "one directory cannot hold more than u32::MAX children"
    );
    // `into_boxed_slice` shrinks to an exact-size allocation when `v` has spare capacity,
    // so a `Vec` built with `reserve_exact` is handed over without a copy.
    let boxed = v.into_boxed_slice();
    let len = boxed.len() as u32;
    let ptr = Box::into_raw(boxed) as *mut Node;
    // SAFETY: `Box::into_raw` never returns null.
    (unsafe { NonNull::new_unchecked(ptr) }, len)
}

/// Free a child allocation produced by [`kids_from_vec`].
///
/// # Safety
///
/// `ptr`/`len` must come from `kids_from_vec` (or be the dangling/zero pair) and this
/// must be the last user of that allocation.
unsafe fn free_kids(ptr: NonNull<Node>, len: u32) {
    if len != 0 {
        // SAFETY: the caller guarantees the pointer and length came from `kids_from_vec`.
        unsafe {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                ptr.as_ptr(),
                len as usize,
            )));
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        // SAFETY: this node owns its children and is being destroyed.
        unsafe { free_kids(self.kids, self.kids_len) }
    }
}

impl Clone for Node {
    fn clone(&self) -> Self {
        let (kids, kids_len) = kids_from_vec(self.children().to_vec());
        Node {
            name: self.name.clone(),
            size: self.size,
            disk_size: self.disk_size,
            meta: self.meta,
            kids,
            kids_len,
            mtime: self.mtime,
        }
    }
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Node")
            .field("kind", &self.kind())
            .field("name", &self.name)
            .field("size", &self.size)
            .field("disk_size", &self.disk_size)
            .field("file_count", &self.file_count())
            .field("dir_count", &self.dir_count())
            .field("errors", &self.errors())
            .field("mtime", &self.mtime)
            .field("children", &self.children())
            .finish()
    }
}

impl PartialEq for Node {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.size == other.size
            && self.disk_size == other.disk_size
            && self.meta == other.meta
            && self.mtime == other.mtime
            && self.children() == other.children()
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
    let idx = node.children().iter().position(|c| *c.name == *names[0])?;
    let removed = if names.len() == 1 {
        let mut v = node.take_children();
        let r = v.remove(idx);
        node.set_children(v);
        r
    } else {
        remove_rec(&mut node.children_mut()[idx], &names[1..])?
    };
    node.size -= removed.size;
    node.disk_size -= removed.disk_size;
    node.set_counts(
        node.file_count().saturating_sub(removed.file_count()),
        node.dir_count()
            .saturating_sub(removed.dir_count() + removed.is_dir() as u32),
        node.errors().saturating_sub(removed.errors()),
    );
    node.children_mut()
        .sort_unstable_by_key(|c| std::cmp::Reverse(c.disk_size));
    Some(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, size: u64) -> Node {
        let mut n = Node::new(NodeKind::File, CompactString::from(name), size, size, 0);
        n.set_counts(1, 0, 0);
        n
    }

    #[test]
    fn node_is_64_bytes() {
        // A big drive holds millions of nodes; keep this from growing by accident.
        assert_eq!(std::mem::size_of::<Node>(), 64);
    }

    #[test]
    fn children_round_trip_and_drop() {
        let mut dir = Node::new(NodeKind::Dir, CompactString::from("d"), 0, 0, 0);
        assert!(dir.children().is_empty());
        dir.set_children(vec![file("a", 1), file("b", 2)]);
        assert_eq!(dir.children().len(), 2);
        assert_eq!(&*dir.children()[1].name, "b");
        dir.children_mut()[0].size = 7;
        assert_eq!(dir.children()[0].size, 7);

        // Replacing children frees the old list; taking them out empties the node.
        dir.set_children(vec![file("c", 3)]);
        assert_eq!(dir.children().len(), 1);
        let v = dir.take_children();
        assert_eq!(v.len(), 1);
        assert!(dir.children().is_empty());

        // A clone has its own copy.
        let mut dir2 = Node::new(NodeKind::Dir, CompactString::from("d"), 0, 0, 0);
        dir2.set_children(v);
        let mut copy = dir2.clone();
        copy.children_mut()[0].size = 99;
        assert_eq!(dir2.children()[0].size, 3);
        assert_eq!(copy.children()[0].size, 99);
    }

    #[test]
    fn counts_saturate() {
        let mut n = Node::new(NodeKind::Dir, CompactString::from("d"), 0, 0, 0);
        n.set_counts(u32::MAX, u32::MAX, u32::MAX);
        assert_eq!(n.file_count(), FILES_MAX);
        assert_eq!(n.dir_count(), DIRS_MAX);
        assert_eq!(n.errors(), ERRORS_MAX);
        n.set_counts(1, 2, ERRORS_MAX);
        n.add_error();
        assert_eq!(n.errors(), ERRORS_MAX); // saturates
        assert_eq!(n.file_count(), 1);
        assert_eq!(n.kind(), NodeKind::Dir);
    }

    #[test]
    fn mtime_round_trips_and_clamps() {
        assert_eq!(mtime_to_u32(i64::MIN), NO_TIME);
        assert_eq!(mtime_to_u32(-5), 0);
        assert_eq!(mtime_to_u32(1_700_000_000), 1_700_000_000);
        let mut n = Node::new(
            NodeKind::File,
            CompactString::from("f"),
            1,
            1,
            mtime_to_u32(1_700_000_000),
        );
        assert_eq!(n.mtime_unix(), 1_700_000_000);
        assert!(n.modified().is_some());
        n.mtime = NO_TIME;
        assert_eq!(n.mtime_unix(), i64::MIN);
        assert!(n.modified().is_none());
        assert!(n.age().is_none());
    }
}
