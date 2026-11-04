use std::fs;
use std::io;
use std::path::Path;
use std::time::{Instant, SystemTime};

use rayon::prelude::*;

use crate::tree::{Node, NodeKind, Scan};

#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    /// Do not descend into other filesystems / mount points. (Not implemented yet, see PLAN.md.)
    pub one_filesystem: bool,
}

/// Scan `root` and return the full tree.
///
/// Symlinks and Windows junctions are recorded but never followed, so a scan
/// cannot loop or count the same data twice through a link.
pub fn scan(root: &Path, _opts: &ScanOptions) -> io::Result<Scan> {
    let started = Instant::now();
    let root = std::path::absolute(root)?;
    let meta = fs::symlink_metadata(&root)?;
    let name = root.to_string_lossy().into_owned();
    let tree = if meta.is_dir() {
        scan_dir(&root, name, meta.modified().ok())
    } else {
        file_node(name, &meta)
    };
    Ok(Scan {
        root,
        tree,
        scanned_at: SystemTime::now(),
        elapsed_ms: started.elapsed().as_millis(),
    })
}

fn file_node(name: String, meta: &fs::Metadata) -> Node {
    Node {
        name,
        kind: NodeKind::File,
        size: if meta.is_file() { meta.len() } else { 0 },
        file_count: 1,
        dir_count: 0,
        modified: meta.modified().ok(),
        errors: 0,
        children: Vec::new(),
    }
}

fn scan_dir(path: &Path, name: String, modified: Option<SystemTime>) -> Node {
    let mut node = Node {
        name,
        kind: NodeKind::Dir,
        size: 0,
        file_count: 0,
        dir_count: 0,
        modified,
        errors: 0,
        children: Vec::new(),
    };

    let entries = match fs::read_dir(path) {
        Ok(rd) => rd,
        Err(_) => {
            node.errors = 1;
            return node;
        }
    };

    let mut subdirs = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            node.errors += 1;
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        // symlink_metadata: never follow links/junctions.
        match entry.path().symlink_metadata() {
            Ok(meta) if meta.is_dir() => subdirs.push((entry.path(), name, meta.modified().ok())),
            Ok(meta) => node.children.push(file_node(name, &meta)),
            Err(_) => node.errors += 1,
        }
    }

    // Recurse into subdirectories in parallel.
    let dirs: Vec<Node> = subdirs
        .into_par_iter()
        .map(|(p, n, m)| scan_dir(&p, n, m))
        .collect();
    node.children.extend(dirs);

    for c in &node.children {
        node.size += c.size;
        node.errors += c.errors;
        match c.kind {
            NodeKind::File => node.file_count += 1,
            NodeKind::Dir => {
                node.dir_count += 1 + c.dir_count;
                node.file_count += c.file_count;
            }
        }
    }
    node.children
        .sort_unstable_by_key(|x| std::cmp::Reverse(x.size));
    node
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn sizes_roll_up() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("a/b/big.bin"), vec![0u8; 1000]).unwrap();
        fs::write(dir.path().join("a/small.txt"), b"hi").unwrap();
        fs::write(dir.path().join("root.txt"), vec![0u8; 10]).unwrap();

        let s = scan(dir.path(), &ScanOptions::default()).unwrap();
        assert_eq!(s.tree.size, 1012);
        assert_eq!(s.tree.file_count, 3);
        assert_eq!(s.tree.dir_count, 2);
        assert_eq!(s.tree.children[0].name, "a"); // largest first
        assert_eq!(s.find(Path::new("a/b")).unwrap().size, 1000);
    }
}
