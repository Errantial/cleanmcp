use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use compact_str::CompactString;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use rayon::prelude::*;

use crate::os::{self, FileId, FsInfo};
use crate::tree::{NO_TIME, Node, NodeKind, Scan, ScanSettings, ScanStats, mtime_to_u32};

/// Live counters and a cancel switch shared with a running scan.
#[derive(Debug, Default)]
pub struct ScanControl {
    pub entries: AtomicU64,
    pub bytes: AtomicU64,
    cancel: AtomicBool,
}

impl ScanControl {
    pub fn cancel(&self) {
        self.cancel.store(true, Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Relaxed)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    /// Do not descend into directories on another filesystem (Unix). On Windows,
    /// mounted volumes are reparse points and are never followed anyway.
    pub one_filesystem: bool,
    /// Also scan `/proc`, `/sys`, `/dev`, `/run` (Linux). Off by default.
    pub include_virtual: bool,
    /// Count hard-linked files once on Windows too (slower: opens every file).
    /// On Unix this is always on because it is free.
    pub dedupe_hardlinks: bool,
    /// Glob patterns to skip, matched against the entry name and its full path
    /// (with `/` separators), e.g. `node_modules` or `**/.git`.
    pub exclude: Vec<String>,
    /// Progress counters / cancellation. Keep a clone to watch or cancel the scan.
    pub control: Arc<ScanControl>,
}

impl ScanOptions {
    /// Options that repeat a previous scan's settings.
    pub fn from_settings(s: &ScanSettings) -> Self {
        ScanOptions {
            one_filesystem: s.one_filesystem,
            include_virtual: s.include_virtual,
            dedupe_hardlinks: s.dedupe_hardlinks,
            exclude: s.exclude.clone(),
            control: Default::default(),
        }
    }

    pub fn settings(&self) -> ScanSettings {
        ScanSettings {
            one_filesystem: self.one_filesystem,
            include_virtual: self.include_virtual,
            dedupe_hardlinks: self.dedupe_hardlinks,
            exclude: self.exclude.clone(),
        }
    }
}

struct Ctx<'a> {
    opts: &'a ScanOptions,
    exclude: Option<GlobSet>,
    fs: FsInfo,
    seen: Mutex<HashSet<FileId>>,
    hardlinks: AtomicU64,
    mounts: AtomicU64,
    excluded: AtomicU64,
}

/// Scan `root` and return the full tree.
///
/// Symlinks and Windows junctions are recorded but never followed, so a scan
/// cannot loop or count the same data twice through a link.
pub fn scan(root: &Path, opts: &ScanOptions) -> io::Result<Scan> {
    let started = Instant::now();
    // The root the user named is resolved (like `du -H`); links *inside* it are not followed.
    let root = crate::resolve_path(root)?;
    let meta = fs::symlink_metadata(&root)?;
    let exclude = build_globs(&opts.exclude)?;
    let ctx = Ctx {
        opts,
        exclude,
        fs: os::fs_info(&root, &meta),
        seen: Mutex::new(HashSet::new()),
        hardlinks: AtomicU64::new(0),
        mounts: AtomicU64::new(0),
        excluded: AtomicU64::new(0),
    };
    let name = CompactString::from(root.to_string_lossy());
    let tree = if meta.is_dir() {
        scan_dir(&ctx, &root, name, mtime(&meta))
    } else {
        file_node(&ctx, &root, name, &meta)
    };
    Ok(Scan {
        root,
        tree,
        scanned_at: unix_now(),
        elapsed_ms: started.elapsed().as_millis() as u64,
        stats: ScanStats {
            hardlinks_deduped: ctx.hardlinks.into_inner(),
            skipped_mounts: ctx.mounts.into_inner(),
            excluded: ctx.excluded.into_inner(),
            cancelled: opts.control.is_cancelled(),
        },
        settings: opts.settings(),
    })
}

fn build_globs(patterns: &[String]) -> io::Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let invalid = |e| io::Error::new(io::ErrorKind::InvalidInput, e);
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        // globset matches Windows paths with `/`, so accept `\` in patterns there too.
        let p = if cfg!(windows) {
            p.replace('\\', "/")
        } else {
            p.clone()
        };
        b.add(
            GlobBuilder::new(&p)
                .case_insensitive(cfg!(windows))
                .build()
                .map_err(invalid)?,
        );
    }
    b.build().map(Some).map_err(invalid)
}

/// Current time as Unix seconds.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn mtime(meta: &fs::Metadata) -> u32 {
    match meta.modified() {
        Ok(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => mtime_to_u32(d.as_secs().min(i64::MAX as u64) as i64),
            // Before 1970: store the epoch (age queries see "very old" either way).
            Err(_) => 0,
        },
        Err(_) => NO_TIME,
    }
}

fn file_node(ctx: &Ctx, path: &Path, name: CompactString, meta: &fs::Metadata) -> Node {
    let ft = meta.file_type();
    let kind = if ft.is_symlink() {
        NodeKind::Link
    } else if ft.is_file() {
        NodeKind::File
    } else {
        NodeKind::Other
    };
    let (mut size, mut disk) = if kind == NodeKind::File {
        (meta.len(), os::disk_size(path, meta, &ctx.fs))
    } else {
        (0, 0)
    };
    if kind == NodeKind::File
        && let Some(id) = os::hardlink_id(path, meta, ctx.opts.dedupe_hardlinks)
        && !ctx.seen.lock().unwrap().insert(id)
    {
        ctx.hardlinks.fetch_add(1, Relaxed);
        size = 0;
        disk = 0;
    }
    ctx.opts.control.entries.fetch_add(1, Relaxed);
    ctx.opts.control.bytes.fetch_add(disk, Relaxed);
    let mut node = Node::new(kind, name, size, disk, mtime(meta));
    node.set_counts((kind == NodeKind::File) as u32, 0, 0);
    node
}

fn is_excluded(ctx: &Ctx, path: &Path, name: &str) -> bool {
    let Some(set) = &ctx.exclude else {
        return false;
    };
    // globset normalizes `\` to `/` itself on Windows.
    set.is_match(name) || set.is_match(path)
}

fn scan_dir(ctx: &Ctx, path: &Path, name: CompactString, mtime: u32) -> Node {
    let mut node = Node::new(NodeKind::Dir, name, 0, 0, mtime);
    ctx.opts.control.entries.fetch_add(1, Relaxed);
    if ctx.opts.control.is_cancelled() {
        return node;
    }

    let entries = match fs::read_dir(path) {
        Ok(rd) => rd,
        Err(_) => {
            node.add_error();
            return node;
        }
    };

    let mut children = Vec::new();
    let mut subdirs: Vec<(PathBuf, CompactString, u32)> = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            node.add_error();
            continue;
        };
        let name = CompactString::from(entry.file_name().to_string_lossy());
        let child_path = entry.path();
        if is_excluded(ctx, &child_path, &name) {
            ctx.excluded.fetch_add(1, Relaxed);
            continue;
        }
        // DirEntry::metadata does not follow links. On Windows it is free (it comes
        // from the directory listing); on Unix it is one lstat.
        match entry.metadata() {
            Ok(meta) if meta.is_dir() => {
                if !ctx.opts.include_virtual && os::is_virtual_fs(&child_path) {
                    ctx.excluded.fetch_add(1, Relaxed);
                    continue;
                }
                if ctx.opts.one_filesystem && os::device(&meta).is_some_and(|d| d != ctx.fs.device)
                {
                    ctx.mounts.fetch_add(1, Relaxed);
                    continue;
                }
                subdirs.push((child_path, name, self::mtime(&meta)));
            }
            Ok(meta) => children.push(file_node(ctx, &child_path, name, &meta)),
            Err(_) => node.add_error(),
        }
    }

    // Recurse into subdirectories in parallel.
    let dirs: Vec<Node> = subdirs
        .into_par_iter()
        .map(|(p, n, m)| scan_dir(ctx, &p, n, m))
        .collect();
    // Size the buffer for exactly what goes in, so `into_boxed_slice` below needs no copy.
    children.reserve_exact(dirs.len());
    children.extend(dirs);
    node.set_children(children);
    node.finish_dir();
    node
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn opts() -> ScanOptions {
        ScanOptions::default()
    }

    #[test]
    fn sizes_roll_up() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("a/b/big.bin"), vec![1u8; 100_000]).unwrap();
        fs::write(dir.path().join("a/small.txt"), b"hi").unwrap();
        fs::write(dir.path().join("root.txt"), vec![1u8; 10]).unwrap();

        let s = scan(dir.path(), &opts()).unwrap();
        assert_eq!(s.tree.size, 100_012);
        assert!(s.tree.disk_size >= 100_000);
        assert_eq!(s.tree.file_count(), 3);
        assert_eq!(s.tree.dir_count(), 2);
        assert_eq!(&*s.tree.children()[0].name, "a"); // largest first
        assert_eq!(s.find(Path::new("a/b")).unwrap().size, 100_000);
        assert_eq!(s.find(&s.root.join("a").join("b")).unwrap().size, 100_000);
    }

    #[test]
    fn exclude_by_name_and_path() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("keep/node_modules")).unwrap();
        fs::write(dir.path().join("keep/node_modules/x"), vec![1u8; 50]).unwrap();
        fs::write(dir.path().join("keep/y.log"), vec![1u8; 50]).unwrap();
        fs::write(dir.path().join("keep/z.txt"), vec![1u8; 7]).unwrap();
        let o = ScanOptions {
            exclude: vec!["node_modules".into(), "**/*.log".into()],
            ..opts()
        };
        let s = scan(dir.path(), &o).unwrap();
        assert_eq!(s.tree.size, 7);
        assert_eq!(s.stats.excluded, 2);
    }

    #[test]
    fn exclude_accepts_backslashes_on_windows() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/skip")).unwrap();
        fs::write(dir.path().join("a/skip/x"), vec![1u8; 50]).unwrap();
        fs::write(dir.path().join("a/y"), vec![1u8; 7]).unwrap();
        let pattern = if cfg!(windows) {
            r"**\a\skip"
        } else {
            "**/a/skip"
        };
        let o = ScanOptions {
            exclude: vec![pattern.into()],
            ..opts()
        };
        assert_eq!(scan(dir.path(), &o).unwrap().tree.size, 7);
    }

    #[test]
    fn apparent_mode_reorders_children() {
        let dir = tempfile::tempdir().unwrap();
        // Many small files: big on disk (cluster rounding), small apparent size.
        fs::create_dir(dir.path().join("many")).unwrap();
        for i in 0..50 {
            fs::write(dir.path().join(format!("many/{i}")), vec![1u8; 600]).unwrap();
        }
        fs::write(dir.path().join("one"), vec![1u8; 40_000]).unwrap();
        let s = scan(dir.path(), &opts()).unwrap();
        let apparent: Vec<_> = s.tree.sorted_children(crate::SizeMode::Apparent);
        assert_eq!(&*apparent[0].name, "one");
        assert_eq!(&*s.tree.children()[0].name, "many"); // disk order
    }

    #[test]
    fn remove_updates_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("a/b/f"), vec![1u8; 500]).unwrap();
        fs::write(dir.path().join("a/g"), vec![1u8; 5]).unwrap();
        let mut s = scan(dir.path(), &opts()).unwrap();
        let removed = s.remove(Path::new("a/b")).unwrap();
        assert_eq!(removed.size, 500);
        assert_eq!(s.tree.size, 5);
        assert_eq!(s.tree.file_count(), 1);
        assert_eq!(s.tree.dir_count(), 1);
        assert!(s.find(Path::new("a/b")).is_none());
    }

    #[test]
    fn child_lists_are_exact_and_counts_packed() {
        // A big drive holds millions of nodes: the node size and the way child
        // lists are built are asserted together (see `tree::tests` for 64 bytes).
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("d1/d2")).unwrap();
        for i in 0..100 {
            fs::write(dir.path().join(format!("f{i}")), vec![1u8; 10]).unwrap();
        }
        fs::write(dir.path().join("d1/x"), vec![1u8; 20]).unwrap();
        let s = scan(dir.path(), &opts()).unwrap();
        // 100 files and 1 dir (d1) at the root; 2 dirs anywhere below it.
        assert_eq!(s.tree.file_count(), 101);
        assert_eq!(s.tree.dir_count(), 2);
        assert_eq!(s.tree.children().len(), 101);
        assert_eq!(s.find(Path::new("d1")).unwrap().file_count(), 1);
        assert_eq!(s.find(Path::new("d1")).unwrap().dir_count(), 1);
        // Children are sorted largest first; 100 small files outweigh one tiny dir.
        assert!(s.tree.children()[0].is_file());
        assert!(s.tree.children().iter().any(|c| c.is_dir()));
    }

    #[test]
    fn cancelled_scan_is_flagged() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f"), b"x").unwrap();
        let o = opts();
        o.control.cancel();
        let s = scan(dir.path(), &o).unwrap();
        assert!(s.stats.cancelled);
    }

    #[cfg(unix)]
    #[test]
    fn hardlinks_counted_once_and_symlinks_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::write(p.join("orig"), vec![1u8; 10_000]).unwrap();
        fs::hard_link(p.join("orig"), p.join("link")).unwrap();
        std::os::unix::fs::symlink(p, p.join("loop")).unwrap();
        let s = scan(p, &opts()).unwrap();
        assert_eq!(s.tree.size, 10_000);
        assert_eq!(s.stats.hardlinks_deduped, 1);
        assert_eq!(s.find(Path::new("loop")).unwrap().kind, NodeKind::Link);
    }

    #[cfg(windows)]
    #[test]
    fn windows_hardlink_dedupe_is_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::write(p.join("orig"), vec![1u8; 10_000]).unwrap();
        fs::hard_link(p.join("orig"), p.join("link")).unwrap();
        assert_eq!(scan(p, &opts()).unwrap().tree.size, 20_000);
        let o = ScanOptions {
            dedupe_hardlinks: true,
            ..opts()
        };
        let s = scan(p, &o).unwrap();
        assert_eq!(s.tree.size, 10_000);
        assert_eq!(s.stats.hardlinks_deduped, 1);
    }
}
