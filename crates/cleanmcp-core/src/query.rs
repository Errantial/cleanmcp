//! Read-only questions about a scanned tree.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::tree::{Node, NodeKind};

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Entry {
    pub path: PathBuf,
    pub size: u64,
    pub is_dir: bool,
    pub file_count: u64,
    /// Seconds since last modification, if known.
    pub age_secs: Option<u64>,
}

impl Entry {
    pub fn from_node(path: &Path, n: &Node) -> Self {
        Entry {
            path: path.to_path_buf(),
            size: n.size,
            is_dir: n.is_dir(),
            file_count: n.file_count,
            age_secs: age(n.modified).map(|d| d.as_secs()),
        }
    }
}

pub fn age(modified: Option<SystemTime>) -> Option<Duration> {
    SystemTime::now().duration_since(modified?).ok()
}

/// The `n` largest files anywhere below `base`, optionally only those older than `min_age`.
pub fn largest_files(root: &Node, base: &Path, n: usize, min_age: Option<Duration>) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    root.walk(base, &mut |p, node| {
        if node.kind == NodeKind::File
            && min_age.is_none_or(|min| age(node.modified).is_some_and(|a| a >= min))
        {
            out.push(Entry::from_node(p, node));
        }
    });
    out.sort_unstable_by_key(|x| std::cmp::Reverse(x.size));
    out.truncate(n);
    out
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ExtensionStat {
    /// Lowercased extension without the dot, or "(none)".
    pub extension: String,
    pub size: u64,
    pub file_count: u64,
}

/// Total size per file extension, largest first.
pub fn by_extension(root: &Node, n: usize) -> Vec<ExtensionStat> {
    let mut map: HashMap<String, (u64, u64)> = HashMap::new();
    root.walk(Path::new(""), &mut |_, node| {
        if node.kind == NodeKind::File {
            let ext = Path::new(&node.name)
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_else(|| "(none)".into());
            let e = map.entry(ext).or_default();
            e.0 += node.size;
            e.1 += 1;
        }
    });
    let mut out: Vec<_> = map
        .into_iter()
        .map(|(extension, (size, file_count))| ExtensionStat {
            extension,
            size,
            file_count,
        })
        .collect();
    out.sort_unstable_by_key(|x| std::cmp::Reverse(x.size));
    out.truncate(n);
    out
}
