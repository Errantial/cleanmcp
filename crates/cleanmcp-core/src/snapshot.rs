//! Save a scan to disk and compare two scans to see what grew.
//!
//! Format: JSON Lines. Line 1 is a header object; every other line is one node
//! in depth-first order: `[depth, kind, name, size, disk_size, mtime, errors]`.
//! A flat format keeps memory low and avoids JSON nesting limits on deep trees.
//! Files ending in `.gz` are gzip-compressed.

use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use compact_str::CompactString;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};

use crate::tree::{Node, NodeKind, Scan, ScanSettings, ScanStats, SizeMode};

const FORMAT: &str = "cleanmcp-snapshot";
const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Header {
    format: String,
    version: u32,
    root: PathBuf,
    scanned_at: i64,
    elapsed_ms: u64,
    #[serde(default)]
    settings: ScanSettings,
    #[serde(default)]
    stats: ScanStats,
}

fn kind_code(k: NodeKind) -> u8 {
    match k {
        NodeKind::File => 0,
        NodeKind::Dir => 1,
        NodeKind::Link => 2,
        NodeKind::Other => 3,
    }
}

fn kind_from(c: u8) -> NodeKind {
    match c {
        0 => NodeKind::File,
        1 => NodeKind::Dir,
        2 => NodeKind::Link,
        _ => NodeKind::Other,
    }
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Write `scan` to `path` (gzip if it ends in `.gz`).
pub fn save(scan: &Scan, path: &Path) -> io::Result<()> {
    let file = File::create(path)?;
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("gz"))
    {
        let mut w = GzEncoder::new(BufWriter::new(file), Compression::fast());
        write_to(scan, &mut w)?;
        w.finish()?.flush()
    } else {
        let mut w = BufWriter::new(file);
        write_to(scan, &mut w)?;
        w.flush()
    }
}

fn write_to(scan: &Scan, w: &mut impl Write) -> io::Result<()> {
    let header = Header {
        format: FORMAT.into(),
        version: VERSION,
        root: scan.root.clone(),
        scanned_at: scan.scanned_at,
        elapsed_ms: scan.elapsed_ms,
        settings: scan.settings.clone(),
        stats: scan.stats.clone(),
    };
    serde_json::to_writer(&mut *w, &header)?;
    w.write_all(b"\n")?;
    // Iterative DFS so deep trees cannot overflow the stack.
    let mut stack: Vec<(u32, &Node)> = vec![(0, &scan.tree)];
    while let Some((depth, n)) = stack.pop() {
        serde_json::to_writer(
            &mut *w,
            &(
                depth,
                kind_code(n.kind()),
                n.name.as_str(),
                n.size,
                n.disk_size,
                n.mtime_unix(),
                own_errors(n),
            ),
        )?;
        w.write_all(b"\n")?;
        for c in n.children().iter().rev() {
            stack.push((depth + 1, c));
        }
    }
    Ok(())
}

/// Errors recorded on this node itself (directories store the subtree total).
fn own_errors(n: &Node) -> u32 {
    let below: u32 = n
        .children()
        .iter()
        .map(|c| c.errors())
        .fold(0, u32::saturating_add);
    n.errors().saturating_sub(below)
}

/// Read a snapshot written by [`save`].
pub fn load(path: &Path) -> io::Result<Scan> {
    let file = File::open(path)?;
    let reader: Box<dyn BufRead> = if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("gz"))
    {
        Box::new(BufReader::new(GzDecoder::new(file)))
    } else {
        Box::new(BufReader::new(file))
    };
    let mut lines = reader.lines();
    let header: Header =
        serde_json::from_str(&lines.next().ok_or_else(|| invalid("empty snapshot"))??)?;
    if header.format != FORMAT || header.version != VERSION {
        return Err(invalid(format!(
            "{} is not a cleanmcp snapshot (version {VERSION})",
            path.display()
        )));
    }

    // Stack of open directories; children are attached when a directory closes.
    let mut stack: Vec<(Node, Vec<Node>)> = Vec::new();
    let mut root: Option<Node> = None;
    let close = |stack: &mut Vec<(Node, Vec<Node>)>, root: &mut Option<Node>| {
        let (mut node, kids) = stack.pop().expect("non-empty");
        node.set_children(kids);
        if node.is_dir() {
            node.finish_dir();
        }
        match stack.last_mut() {
            Some((_, siblings)) => siblings.push(node),
            None => *root = Some(node),
        }
    };
    for line in lines {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let (depth, kind, name, size, disk, mtime, errors): (
            usize,
            u8,
            CompactString,
            u64,
            u64,
            i64,
            u32,
        ) = serde_json::from_str(&line)?;
        if depth > stack.len() || (depth == 0 && (root.is_some() || !stack.is_empty())) {
            return Err(invalid("snapshot nodes out of order"));
        }
        while stack.len() > depth {
            close(&mut stack, &mut root);
        }
        let kind = kind_from(kind);
        let mut node = Node::new(kind, name, size, disk, crate::tree::mtime_to_u32(mtime));
        node.set_counts((kind == NodeKind::File) as u32, 0, errors);
        stack.push((node, Vec::new()));
    }
    while !stack.is_empty() {
        close(&mut stack, &mut root);
    }
    Ok(Scan {
        root: header.root,
        tree: root.ok_or_else(|| invalid("snapshot has no nodes"))?,
        scanned_at: header.scanned_at,
        elapsed_ms: header.elapsed_ms,
        stats: header.stats,
        settings: header.settings,
    })
}

/// How one path changed between two scans.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DiffNode {
    pub path: PathBuf,
    pub old_size: u64,
    pub new_size: u64,
    /// `new_size - old_size` (negative = shrank).
    pub delta: i64,
    /// "added", "removed" or "changed".
    pub status: &'static str,
    pub is_dir: bool,
    /// Changed children, largest absolute change first (limited by depth/top).
    pub children: Vec<DiffNode>,
    /// Number of changed children not listed because of `top`.
    pub more: usize,
}

/// Compare two trees. Only paths whose size changed are included.
pub fn diff(
    old: &Node,
    new: &Node,
    base: &Path,
    depth: usize,
    top: usize,
    mode: SizeMode,
) -> DiffNode {
    diff_rec(Some(old), Some(new), base, depth, top, mode)
}

fn diff_rec(
    old: Option<&Node>,
    new: Option<&Node>,
    path: &Path,
    depth: usize,
    top: usize,
    mode: SizeMode,
) -> DiffNode {
    let old_size = old.map_or(0, |n| n.size_by(mode));
    let new_size = new.map_or(0, |n| n.size_by(mode));
    let status = match (old, new) {
        (None, _) => "added",
        (_, None) => "removed",
        _ => "changed",
    };
    let is_dir = new.or(old).is_some_and(|n| n.is_dir());
    let mut children = Vec::new();
    let mut more = 0;
    if depth > 0 && is_dir {
        let mut changed: Vec<(i64, Option<&Node>, Option<&Node>)> = Vec::new();
        let olds: std::collections::HashMap<&str, &Node> = old
            .map(|n| n.children().iter().map(|c| (c.name.as_str(), c)).collect())
            .unwrap_or_default();
        let mut seen = std::collections::HashSet::new();
        for c in new.map(|n| n.children()).unwrap_or_default() {
            seen.insert(c.name.as_str());
            let o = olds.get(c.name.as_str()).copied();
            let d = c.size_by(mode) as i64 - o.map_or(0, |o| o.size_by(mode) as i64);
            if d != 0 || o.is_none() {
                changed.push((d, o, Some(c)));
            }
        }
        for (name, o) in &olds {
            if !seen.contains(name) {
                changed.push((-(o.size_by(mode) as i64), Some(*o), None));
            }
        }
        changed.sort_by_key(|(d, ..)| std::cmp::Reverse(d.unsigned_abs()));
        more = changed.len().saturating_sub(top);
        children = changed
            .into_iter()
            .take(top)
            .map(|(_, o, n)| {
                let name = n.or(o).map(|x| x.name.as_str()).unwrap_or_default();
                diff_rec(o, n, &path.join(name), depth - 1, top, mode)
            })
            .collect();
    }
    DiffNode {
        path: path.to_path_buf(),
        old_size,
        new_size,
        delta: new_size as i64 - old_size as i64,
        status,
        is_dir,
        children,
        more,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{ScanOptions, scan};
    use std::fs;

    #[test]
    fn roundtrip_and_diff() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("data");
        fs::create_dir_all(p.join("a/deep/er")).unwrap();
        fs::write(p.join("a/deep/er/f"), vec![1u8; 1000]).unwrap();
        fs::write(p.join("gone"), vec![1u8; 300]).unwrap();
        let before = scan(&p, &ScanOptions::default()).unwrap();

        for name in ["snap.jsonl", "snap.jsonl.gz"] {
            let file = dir.path().join(name);
            save(&before, &file).unwrap();
            let loaded = load(&file).unwrap();
            assert_eq!(loaded.root, before.root);
            assert_eq!(loaded.tree.size, before.tree.size);
            assert_eq!(loaded.tree.file_count(), before.tree.file_count());
            assert_eq!(loaded.tree.dir_count(), before.tree.dir_count());
            assert_eq!(loaded.settings, before.settings);
            assert_eq!(loaded.find(Path::new("a/deep/er/f")).unwrap().size, 1000);
        }

        fs::remove_file(p.join("gone")).unwrap();
        fs::write(p.join("a/new"), vec![1u8; 5000]).unwrap();
        let after = scan(&p, &ScanOptions::default()).unwrap();
        let d = diff(
            &before.tree,
            &after.tree,
            &after.root,
            3,
            10,
            SizeMode::Apparent,
        );
        assert_eq!(d.delta, 5000 - 300);
        assert_eq!(d.children[0].status, "changed"); // a/ grew by 5000
        assert_eq!(d.children[0].children[0].status, "added");
        assert_eq!(d.children[1].status, "removed");
    }
}
