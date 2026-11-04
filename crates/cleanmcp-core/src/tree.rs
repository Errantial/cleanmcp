use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Dir,
}

/// One file or directory in a scanned tree.
///
/// For directories, `size`, `file_count` and `dir_count` include everything below them.
#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    pub kind: NodeKind,
    /// Logical size in bytes (file length, not allocated size on disk).
    pub size: u64,
    pub file_count: u64,
    pub dir_count: u64,
    pub modified: Option<SystemTime>,
    /// Entries below this node that could not be read (permission denied, etc.).
    pub errors: u64,
    /// Children sorted by size, largest first. Empty for files.
    pub children: Vec<Node>,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        self.kind == NodeKind::Dir
    }

    /// Find a descendant by path relative to this node, e.g. `"src/main.rs"`.
    pub fn find(&self, rel: &Path) -> Option<&Node> {
        let mut node = self;
        for part in rel.components() {
            let name = part.as_os_str().to_string_lossy();
            if name == "." {
                continue;
            }
            node = node.children.iter().find(|c| c.name == name)?;
        }
        Some(node)
    }

    /// Depth-first walk over every node, giving each node's full path.
    pub fn walk<'a>(&'a self, base: &Path, f: &mut impl FnMut(&Path, &'a Node)) {
        f(base, self);
        for child in &self.children {
            child.walk(&base.join(&child.name), f);
        }
    }
}

/// A scanned tree plus the root path it was scanned from.
#[derive(Debug, Clone)]
pub struct Scan {
    pub root: PathBuf,
    pub tree: Node,
    pub scanned_at: SystemTime,
    pub elapsed_ms: u128,
}

impl Scan {
    /// Look up a node by absolute path or path relative to the scan root.
    pub fn find(&self, path: &Path) -> Option<&Node> {
        let rel = if path.is_absolute() {
            path.strip_prefix(&self.root).ok()?
        } else {
            path
        };
        self.tree.find(rel)
    }
}
