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
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use rayon::prelude::*;

use crate::os::{self, FileId, FsInfo};
use crate::tree::{Node, NodeKind, Scan, ScanSettings, ScanStats};

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
    /// Do not respect `.gitignore` / `.ignore` files while scanning.
    pub no_ignore: bool,
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
            no_ignore: s.no_ignore,
            control: Default::default(),
        }
    }

    pub fn settings(&self) -> ScanSettings {
        ScanSettings {
            one_filesystem: self.one_filesystem,
            include_virtual: self.include_virtual,
            dedupe_hardlinks: self.dedupe_hardlinks,
            exclude: self.exclude.clone(),
            no_ignore: self.no_ignore,
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
    /// Set only where the walk stops before finishing, so `stats.cancelled`
    /// means "totals are partial", not "someone pressed stop at some point".
    truncated: AtomicBool,
}

/// Scan `root` and return the full tree.
///
/// Symlinks and Windows junctions are recorded but never followed, so a scan
/// cannot loop or count the same data twice through a link.
pub fn scan(root: &Path, opts: &ScanOptions) -> io::Result<Scan> {
    let started = Instant::now();
    let started_at = unix_now();
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
        truncated: AtomicBool::new(false),
    };
    let name = root.to_string_lossy();
    let mtime = os::meta_mtime(&meta);
    let no_ig: IgnoreStack = Vec::new();
    let tree = if meta.is_dir() {
        scan_dir(
            &ctx,
            &root,
            CompactString::from(name.as_ref()),
            mtime,
            &no_ig,
        )
    } else {
        let entry = os::FsEntry::from_meta(&name, root.as_os_str(), &meta, mtime);
        let node = file_node(&ctx, &root, &entry);
        ctx.opts.control.entries.fetch_add(1, Relaxed);
        ctx.opts.control.bytes.fetch_add(node.disk_size, Relaxed);
        node
    };
    Ok(Scan {
        root,
        tree,
        scanned_at: unix_now(),
        started_at,
        elapsed_ms: started.elapsed().as_millis() as u64,
        stats: ScanStats {
            hardlinks_deduped: ctx.hardlinks.into_inner(),
            skipped_mounts: ctx.mounts.into_inner(),
            excluded: ctx.excluded.into_inner(),
            cancelled: ctx.truncated.into_inner(),
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

/// Builds `dir/name` in one reusable buffer, so per-entry paths cost no allocation.
struct ChildPath {
    buf: PathBuf,
    base: usize,
    pushed: bool,
}

impl ChildPath {
    fn new(dir: &Path) -> Self {
        ChildPath {
            buf: dir.to_path_buf(),
            base: dir.as_os_str().len(),
            pushed: false,
        }
    }

    /// Point the buffer at `dir/name`; call [`ChildPath::reset`] before the next entry.
    fn set(&mut self, e: &os::FsEntry<'_>) -> &Path {
        e.push_name(&mut self.buf);
        self.pushed = true;
        &self.buf
    }

    fn reset(&mut self) {
        if self.pushed {
            // A listing never yields `.`/`..`, so this drops exactly the pushed name.
            self.buf.pop();
            debug_assert_eq!(self.buf.as_os_str().len(), self.base);
            self.pushed = false;
        }
    }
}

fn file_node(ctx: &Ctx, path: &Path, e: &os::FsEntry<'_>) -> Node {
    let kind = e.kind;
    let (mut size, mut disk) = (e.size, 0);
    if kind == NodeKind::File {
        disk = match e.disk_size_known(&ctx.fs) {
            Some(d) => d,
            None => os::ask_fs(path).unwrap_or_else(|| e.disk_size_estimate(&ctx.fs)),
        };
        if let Some(id) = e.hardlink_id(path, ctx.opts.dedupe_hardlinks)
            && !ctx.seen.lock().unwrap().insert(id)
        {
            ctx.hardlinks.fetch_add(1, Relaxed);
            size = 0;
            disk = 0;
        }
    }
    let mut node = Node::new(kind, CompactString::from(e.name), size, disk, e.mtime);
    node.set_counts((kind == NodeKind::File) as u32, 0, 0);
    node
}

/// Active ignore rules on the way down from the scan root: one entry per
/// directory that had a `.gitignore` or `.ignore` file, with the directory as
/// the matcher's root. Deeper entries override shallower ones, like git.
type IgnoreStack = Vec<(PathBuf, Arc<Gitignore>)>;

/// Build the matcher for `dir/.gitignore` (or `.ignore`). `None` when the file
/// cannot be read: an unreadable ignore file must not fail the whole scan.
fn build_gitignore(dir: &Path, file: &str) -> Option<Arc<Gitignore>> {
    let mut b = GitignoreBuilder::new(dir);
    let _ = b.case_insensitive(cfg!(windows));
    if b.add(dir.join(file)).is_some() {
        return None; // unreadable ignore file: no rules, but do not fail the scan
    }
    b.build().ok().map(Arc::new)
}

/// git semantics: the deepest matcher that says anything wins. `matched`
/// strips the matcher's root itself and reports `None` for paths outside it.
fn is_ignored(ig: &IgnoreStack, path: &Path, is_dir: bool) -> bool {
    let mut ignored = false;
    for (_, g) in ig {
        let m = g.matched(path, is_dir);
        if m.is_ignore() {
            ignored = true;
        } else if m.is_whitelist() {
            ignored = false;
        }
    }
    ignored
}

fn scan_dir(
    ctx: &Ctx,
    path: &Path,
    name: CompactString,
    mtime: u32,
    parent_ig: &IgnoreStack,
) -> Node {
    let mut node = Node::new(NodeKind::Dir, name, 0, 0, mtime);
    // Progress counters are batched per directory: two atomic updates instead of
    // two per entry, which keeps 24 threads off the same cache line.
    let mut entries = 1u64;
    let mut bytes = 0u64;
    let flush = |entries: u64, bytes: u64| {
        let control = &ctx.opts.control;
        control.entries.fetch_add(entries, Relaxed);
        control.bytes.fetch_add(bytes, Relaxed);
    };
    if ctx.opts.control.is_cancelled() {
        ctx.truncated.store(true, Relaxed);
        flush(entries, bytes);
        return node;
    }

    let mut children: Vec<Node> = Vec::new();
    let mut subdirs: Vec<(PathBuf, CompactString, u32)> = Vec::new();
    let mut scratch = ChildPath::new(path);
    let mut excluded = 0u64;
    let mut mounts = 0u64;
    // Ignore files are found in the listing itself, so the common case (no
    // ignore files anywhere) costs two string compares per entry and no syscalls.
    let mut has_gitignore = false;
    let mut has_ignore = false;

    let walked = os::walk_dir(path, &mut |e| {
        // Reset from the previous entry before appending this one.
        scratch.reset();
        let child = scratch.set(e);
        if !ctx.opts.no_ignore && e.kind == NodeKind::File {
            if e.name == ".gitignore" {
                has_gitignore = true;
            } else if e.name == ".ignore" {
                has_ignore = true;
            }
        }
        // globset normalizes `\` to `/` itself on Windows.
        if let Some(set) = &ctx.exclude
            && (set.is_match(e.name) || set.is_match(child))
        {
            excluded += 1;
            return;
        }
        match e.kind {
            NodeKind::Dir => {
                if !ctx.opts.include_virtual && os::is_virtual_fs(child) {
                    excluded += 1;
                    return;
                }
                if ctx.opts.one_filesystem && e.device().is_some_and(|d| d != ctx.fs.device) {
                    mounts += 1;
                    return;
                }
                subdirs.push((child.to_path_buf(), CompactString::from(e.name), e.mtime));
            }
            _ => {
                let n = file_node(ctx, child, e);
                entries += 1;
                bytes += n.disk_size;
                children.push(n);
            }
        }
    });
    match walked {
        Ok(errs) => node.set_counts(node.file_count(), node.dir_count(), errs as u32),
        Err(_) => node.add_error(),
    }

    // Load this directory's ignore files (if the listing had any) and filter
    // the entries collected above; ancestors' rules apply too.
    let mut ig = parent_ig.clone();
    if !ctx.opts.no_ignore {
        for (present, file) in [(&has_gitignore, ".gitignore"), (&has_ignore, ".ignore")] {
            if *present && let Some(g) = build_gitignore(path, file) {
                ig.push((path.to_path_buf(), g));
            }
        }
    }
    if !ig.is_empty() {
        let before = children.len() + subdirs.len();
        children.retain(|n| !is_ignored(&ig, &path.join(n.name.as_str()), false));
        subdirs.retain(|(p, _, _)| !is_ignored(&ig, p, true));
        excluded += (before - children.len() - subdirs.len()) as u64;
    }

    // Recurse into subdirectories in parallel.
    let dirs: Vec<Node> = subdirs
        .into_par_iter()
        .map(|(p, n, m)| scan_dir(ctx, &p, n, m, &ig))
        .collect();
    children.extend(dirs);
    node.set_children(children);
    node.finish_dir();
    ctx.excluded.fetch_add(excluded, Relaxed);
    ctx.mounts.fetch_add(mounts, Relaxed);
    flush(entries, bytes);
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
    fn gitignore_and_ignore_files_are_respected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::create_dir_all(p.join("src")).unwrap();
        fs::create_dir_all(p.join("target/debug")).unwrap();
        fs::write(p.join(".gitignore"), "target/\n*.log\n").unwrap();
        fs::write(p.join("target/debug/app"), vec![1u8; 100]).unwrap();
        fs::write(p.join("src/main.rs"), vec![1u8; 10]).unwrap();
        fs::write(p.join("notes.log"), vec![1u8; 5]).unwrap();

        let s = scan(p, &opts()).unwrap();
        // .gitignore and src/main.rs remain; target/ and *.log are ignored.
        assert_eq!(s.tree.file_count(), 2);
        assert!(s.tree.size < 100); // the 100 B binary and the log are gone
        assert!(s.find(Path::new("target")).is_none());
        assert!(s.stats.excluded >= 2);

        // --no-ignore scans everything.
        let o = ScanOptions {
            no_ignore: true,
            ..opts()
        };
        let s2 = scan(p, &o).unwrap();
        assert_eq!(s2.tree.file_count(), 4);
        assert!(s2.find(Path::new("target/debug/app")).is_some());
    }

    #[test]
    fn nested_gitignore_overrides_and_negation() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::create_dir_all(p.join("sub")).unwrap();
        fs::write(p.join(".gitignore"), "*.tmp\n").unwrap();
        fs::write(p.join("sub/.gitignore"), "!keep.tmp\n").unwrap();
        fs::write(p.join("a.tmp"), vec![1u8; 10]).unwrap();
        fs::write(p.join("sub/keep.tmp"), vec![1u8; 10]).unwrap();
        fs::write(p.join("sub/b.tmp"), vec![1u8; 10]).unwrap();

        let s = scan(p, &opts()).unwrap();
        // a.tmp and sub/b.tmp ignored; sub/keep.tmp whitelisted by the deeper file.
        assert!(s.find(Path::new("a.tmp")).is_none());
        assert!(s.find(Path::new("sub/b.tmp")).is_none());
        assert!(s.find(Path::new("sub/keep.tmp")).is_some());
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
    fn node_stays_small() {
        // A big drive holds millions of nodes; keep this from growing by accident.
        assert!(
            std::mem::size_of::<Node>() <= 80,
            "{}",
            std::mem::size_of::<Node>()
        );
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

    #[test]
    fn many_entries_in_one_directory() {
        // More entries than a single directory-listing buffer holds.
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big");
        fs::create_dir(&big).unwrap();
        for i in 0..800 {
            let name = format!("{i:03}-{}", "x".repeat(60));
            fs::write(big.join(name), vec![1u8; 100]).unwrap();
        }
        let s = scan(dir.path(), &opts()).unwrap();
        assert_eq!(s.tree.file_count(), 800);
        assert_eq!(s.tree.size, 80_000);
        let big = s.find(Path::new("big")).unwrap();
        assert_eq!(big.file_count(), 800);
        assert_eq!(big.children().len(), 800);
    }

    #[test]
    fn unicode_and_long_names() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("hÃ©llo wÃ¶rld - Ã¼nicode.txt"), b"hi").unwrap();
        fs::write(dir.path().join("ðŸš€ emoji.txt"), b"hey").unwrap();
        fs::write(dir.path().join("n".repeat(200)), b"x").unwrap();
        let s = scan(dir.path(), &opts()).unwrap();
        assert_eq!(s.tree.file_count(), 3);
        assert_eq!(s.tree.size, 6);
        assert!(s.find(Path::new("hÃ©llo wÃ¶rld - Ã¼nicode.txt")).is_some());
        assert!(s.find(Path::new("ðŸš€ emoji.txt")).is_some());
        assert!(s.find(Path::new(&"n".repeat(200))).is_some());
    }

    #[test]
    fn paths_longer_than_max_path() {
        // Windows only limits paths that are not given the `\\?\` prefix.
        let dir = tempfile::tempdir().unwrap();
        let mut deep = dir.path().to_path_buf();
        for i in 0..6 {
            deep.push(format!("{i:02}-{}", "d".repeat(40)));
        }
        assert!(deep.as_os_str().len() > 260, "{}", deep.display());
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("file.txt"), b"12345").unwrap();
        let s = scan(dir.path(), &opts()).unwrap();
        assert_eq!(s.tree.file_count(), 1);
        assert_eq!(s.tree.size, 5);
        assert_eq!(s.tree.dir_count(), 6);
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
        assert_eq!(s.find(Path::new("loop")).unwrap().kind(), NodeKind::Link);
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
