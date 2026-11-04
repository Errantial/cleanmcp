//! Read-only questions about a scanned tree.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use crate::tree::{Node, NodeKind, SizeMode};

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
            file_count: n.file_count as u64,
            age_days: n.age().map(|d| d.as_secs() / 86_400),
        }
    }

    pub fn size_by(&self, mode: SizeMode) -> u64 {
        match mode {
            SizeMode::Disk => self.disk_size,
            SizeMode::Apparent => self.size,
        }
    }
}

/// The `n` largest files anywhere below `base`, optionally only those older than `min_age`.
pub fn largest_files(
    root: &Node,
    base: &Path,
    n: usize,
    min_age: Option<Duration>,
    mode: SizeMode,
) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    root.walk(base, &mut |p, node| {
        if node.kind == NodeKind::File
            && min_age.is_none_or(|min| node.age().is_some_and(|a| a >= min))
        {
            out.push(Entry::from_node(p, node));
        }
    });
    out.sort_unstable_by_key(|e| std::cmp::Reverse(e.size_by(mode)));
    out.truncate(n);
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
        if node.kind == NodeKind::File {
            let ext = Path::new(&*node.name)
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_else(|| "(none)".into());
            if !map.contains_key(&ext) {
                map.insert(
                    ext.clone(),
                    ExtensionStat {
                        extension: ext.clone(),
                        disk_size: 0,
                        size: 0,
                        file_count: 0,
                    },
                );
            }
            let e = map.get_mut(&ext).expect("just inserted");
            e.disk_size += node.disk_size;
            e.size += node.size;
            e.file_count += 1;
        }
    });
    let mut out: Vec<_> = map.into_values().collect();
    out.sort_unstable_by_key(|s| std::cmp::Reverse(s.size_by(mode)));
    out.truncate(n);
    out
}
