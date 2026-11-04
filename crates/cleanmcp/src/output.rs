//! Human-readable and JSON output for CLI commands.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use bytesize::ByteSize;
use cleanmcp_core::drives::Drive;
use cleanmcp_core::dupes::DuplicateGroup;
use cleanmcp_core::jsonwrite::{push_str, push_u64};
use cleanmcp_core::query::{AgeStat, Entry, ExtensionStat, format_time};
use cleanmcp_core::rules::{Candidate, Rule};
use cleanmcp_core::snapshot::DiffNode;
use cleanmcp_core::{Node, NodeKind, Scan, SizeMode};
use serde::Serialize;

pub fn human(bytes: u64) -> String {
    ByteSize(bytes).display().iec().to_string()
}

pub fn pct(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 * 100.0 / total as f64
    }
}

pub fn print_json<T: Serialize + ?Sized>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

pub fn print_tree(s: &Scan, depth: usize, top: usize, mode: SizeMode) {
    let label = match mode {
        SizeMode::Disk => "on disk",
        SizeMode::Apparent => "apparent",
    };
    println!(
        "{} {label}  {}  ({} files, {} dirs, {} ms)",
        human(s.tree.size_by(mode)),
        s.root.display(),
        s.tree.file_count(),
        s.tree.dir_count(),
        s.elapsed_ms
    );
    print_children(&s.tree, "", depth, top, mode);
}

pub fn print_scan_notes(s: &Scan) {
    let st = &s.stats;
    if s.tree.errors() > 0 {
        eprintln!(
            "note: {} entries could not be read (permission denied?)",
            s.tree.errors()
        );
    }
    if st.hardlinks_deduped > 0 {
        eprintln!(
            "note: {} extra hard links counted once",
            st.hardlinks_deduped
        );
    }
    if st.skipped_mounts > 0 {
        eprintln!(
            "note: {} mount points skipped (--one-filesystem)",
            st.skipped_mounts
        );
    }
    if st.excluded > 0 {
        eprintln!("note: {} entries excluded", st.excluded);
    }
    if st.cancelled {
        eprintln!("note: scan was cancelled; totals are partial");
    }
}

fn print_children(node: &Node, indent: &str, depth: usize, top: usize, mode: SizeMode) {
    if depth == 0 {
        return;
    }
    let children = node.sorted_children(mode);
    let more = children.len() > top;
    let shown = children.len().min(top);
    let total = node.size_by(mode);
    for (i, c) in children.iter().take(top).enumerate() {
        let last = i + 1 == shown && !more;
        let (branch, next) = if last {
            ("└─ ", "   ")
        } else {
            ("├─ ", "│  ")
        };
        let suffix = match c.kind() {
            NodeKind::Dir => "/",
            NodeKind::Link => " ->",
            _ => "",
        };
        println!(
            "{indent}{branch}{:>10} {:>5.1}%  {}{suffix}",
            human(c.size_by(mode)),
            pct(c.size_by(mode), total),
            c.name
        );
        if c.is_dir() {
            print_children(c, &format!("{indent}{next}"), depth - 1, top, mode);
        }
    }
    if more {
        let rest: u64 = children[top..].iter().map(|c| c.size_by(mode)).sum();
        println!(
            "{indent}└─ {:>10} {:>5.1}%  ({} more)",
            human(rest),
            pct(rest, total),
            children.len() - top
        );
    }
}

pub fn print_entries(entries: &[Entry], mode: SizeMode) {
    for e in entries {
        let age = e.age_days.map(|d| format!("{d}d")).unwrap_or_default();
        println!(
            "{:>10}  {:>6}  {}",
            human(e.size_by(mode)),
            age,
            e.path.display()
        );
    }
}

pub fn print_extensions(stats: &[ExtensionStat], root: &Node, mode: SizeMode) {
    for s in stats {
        let size = s.size_by(mode);
        println!(
            "{:>10} {:>5.1}%  {:>8} files  .{}",
            human(size),
            pct(size, root.size_by(mode)),
            s.file_count,
            s.extension
        );
    }
}

/// JSON document for `suggest --json`: totals over every match, plus the list.
#[derive(Serialize, schemars::JsonSchema)]
pub struct SuggestJson {
    pub total_count: usize,
    pub total_size: u64,
    pub total_size_human: String,
    pub candidates: Vec<Candidate>,
}

impl SuggestJson {
    pub fn build(c: Vec<Candidate>, mode: SizeMode) -> Self {
        let total_size = c.iter().map(|x| x.entry.size_by(mode)).sum();
        SuggestJson {
            total_count: c.len(),
            total_size,
            total_size_human: human(total_size),
            candidates: c,
        }
    }
}

pub fn print_candidates(c: &[Candidate], mode: SizeMode) {
    if c.is_empty() {
        println!("No cleanup candidates found.");
        return;
    }
    let total: u64 = c.iter().map(|c| c.entry.size_by(mode)).sum();
    for c in c {
        println!(
            "{:>10}  {:<11}  {}\n            {}",
            human(c.entry.size_by(mode)),
            format!("{:?}", c.safety).to_lowercase(),
            c.entry.path.display(),
            c.reason
        );
        if let Some(a) = &c.advice {
            println!("            how: {a}");
        }
    }
    println!(
        "\nTotal: {} in {} locations. Nothing was deleted.",
        human(total),
        c.len()
    );
}

/// Tree shape used for `--json` output and the MCP `list_children` tool.
#[derive(Serialize, schemars::JsonSchema)]
pub struct TreeJson {
    pub path: PathBuf,
    /// Allocated size on disk in bytes.
    pub disk_size: u64,
    /// Logical (apparent) size in bytes.
    pub size: u64,
    pub is_dir: bool,
    pub file_count: u64,
    pub age_days: Option<u64>,
    pub errors: u32,
    /// Space used on disk by the children this node did not list: the ones
    /// beyond `top`, or all of them when the depth limit stopped the walk.
    pub other_disk_size: u64,
    /// The same omitted children, in the requested size mode.
    pub other_size: u64,
    /// Children this node did not list (beyond `top`, or all of them when the
    /// depth limit stopped the walk).
    pub other_count: usize,
    /// `true` when the folder has children that are not in `children` (depth
    /// limit or `top`), so an empty `children` does not mean an empty folder.
    /// Files and genuinely empty folders are never truncated.
    pub children_truncated: bool,
    pub children: Vec<TreeJson>,
}

impl TreeJson {
    pub fn build(node: &Node, path: &Path, depth: usize, top: usize, mode: SizeMode) -> Self {
        Self::build_page(node, path, depth, top, 0, mode)
    }

    /// Like [`build`], skipping the first `offset` children at the top level.
    /// `other_*` then counts the children after this page.
    pub fn build_page(
        node: &Node,
        path: &Path,
        depth: usize,
        top: usize,
        offset: usize,
        mode: SizeMode,
    ) -> Self {
        let (children, other_disk_size, other_size, other_count, children_truncated) = if depth == 0
        {
            if node.children().is_empty() {
                (Vec::new(), 0, 0, 0, false)
            } else {
                // The depth limit stopped the walk here. The node's rolled-up
                // totals are exactly its (unlisted) children's totals.
                (
                    Vec::new(),
                    node.disk_size,
                    node.size_by(mode),
                    node.children().len(),
                    true,
                )
            }
        } else {
            let sorted = node.sorted_children(mode);
            let start = offset.min(sorted.len());
            let kids = sorted[start..]
                .iter()
                .take(top)
                .map(|c| TreeJson::build(c, &path.join(c.name.as_str()), depth - 1, top, mode))
                .collect();
            let rest = sorted.get(start + top..).unwrap_or_default();
            (
                kids,
                rest.iter().map(|c| c.disk_size).sum(),
                rest.iter().map(|c| c.size_by(mode)).sum(),
                rest.len(),
                !rest.is_empty(),
            )
        };
        TreeJson {
            path: path.to_path_buf(),
            disk_size: node.disk_size,
            size: node.size,
            is_dir: node.is_dir(),
            file_count: node.file_count() as u64,
            age_days: node.age().map(|d| d.as_secs() / 86_400),
            errors: node.errors(),
            other_disk_size,
            other_size,
            other_count,
            children_truncated,
            children,
        }
    }

    /// Every path in this tree (for the MCP "seen paths" list).
    pub fn collect_paths(&self, out: &mut Vec<PathBuf>) {
        out.push(self.path.clone());
        for c in &self.children {
            c.collect_paths(out);
        }
    }
}

pub fn print_age(stats: &[AgeStat], mode: SizeMode) {
    let size = |s: &AgeStat| match mode {
        SizeMode::Disk => s.disk_size,
        SizeMode::Apparent => s.size,
    };
    let total: u64 = stats.iter().map(size).sum();
    for s in stats.iter().filter(|s| s.file_count > 0) {
        let bar = "#".repeat((pct(size(s), total) / 4.0).round() as usize);
        println!(
            "{:>12}  {:>10} {:>5.1}%  {:>9} files  {bar}",
            s.bucket,
            human(size(s)),
            pct(size(s), total),
            s.file_count
        );
    }
}

/// JSON document for `dupes --json`: totals for every group found, plus the
/// (possibly truncated) page in `groups`.
#[derive(Serialize, schemars::JsonSchema)]
pub struct DupesJson {
    /// Duplicate groups found, before `--top` (and therefore possibly more than
    /// `groups.len()`).
    pub total_groups: usize,
    /// Bytes freed by keeping one copy of every group, in the requested size
    /// mode (disk by default, `--apparent` for file lengths).
    pub total_wasted: u64,
    pub groups: Vec<DuplicateGroup>,
}

impl DupesJson {
    /// Sort by wasted bytes in `mode`, keep `top` groups, and keep the totals
    /// of all groups found.
    pub fn build(groups: Vec<DuplicateGroup>, top: usize, mode: SizeMode) -> Self {
        let mut groups = groups;
        let total_groups = groups.len();
        let total_wasted: u64 = groups.iter().map(|g| g.wasted_by(mode)).sum();
        groups.sort_unstable_by(|a, b| {
            b.wasted_by(mode)
                .cmp(&a.wasted_by(mode))
                .then_with(|| a.paths.cmp(&b.paths))
        });
        groups.truncate(top);
        DupesJson {
            total_groups,
            total_wasted,
            groups,
        }
    }
}

pub fn print_dupes(
    groups: &[DuplicateGroup],
    total_groups: usize,
    total_wasted: u64,
    mode: SizeMode,
) {
    if groups.is_empty() {
        if total_groups == 0 {
            println!("No duplicate files found.");
        } else {
            println!("No duplicate groups shown ({total_groups} found; raise --top).");
        }
        return;
    }
    for g in groups {
        println!(
            "{} wasted  ({} copies of {})",
            human(g.wasted_by(mode)),
            g.paths.len(),
            human(g.size_by(mode))
        );
        for p in &g.paths {
            println!("    {}", p.display());
        }
    }
    println!(
        "\n{} duplicate groups, {} could be freed by keeping one copy of each.",
        total_groups,
        human(total_wasted)
    );
}

pub fn print_rules(rules: &[Rule]) {
    for r in rules {
        let what = match (&r.dir_name, &r.path) {
            (Some(n), _) if r.requires_sibling.is_empty() => format!("dir {n}"),
            (Some(n), _) => format!("dir {n} (next to {})", r.requires_sibling.join("|")),
            (None, Some(p)) => p.clone(),
            _ => String::new(),
        };
        println!(
            "{:<22} {:<11} {:<45} {}",
            r.id,
            format!("{:?}", r.safety).to_lowercase(),
            what,
            r.reason
        );
    }
}

pub fn print_drives(drives: &[Drive]) {
    println!(
        "{:<24} {:>10} {:>10} {:>10} {:>6}  FS",
        "MOUNT", "SIZE", "USED", "FREE", "USE%"
    );
    for d in drives {
        println!(
            "{:<24} {:>10} {:>10} {:>10} {:>5.1}%  {}{}",
            d.mount_point.display(),
            human(d.total),
            human(d.used),
            human(d.available),
            pct(d.used, d.total),
            d.file_system,
            if d.removable { " (removable)" } else { "" }
        );
    }
}

fn signed(delta: i64) -> String {
    let sign = if delta < 0 { "-" } else { "+" };
    format!("{sign}{}", human(delta.unsigned_abs()))
}

pub fn print_diff(d: &DiffNode, old_at: i64, new_at: i64) {
    println!(
        "{}  {}  ({} -> {}, {} UTC -> {} UTC)",
        signed(d.delta),
        d.path.display(),
        human(d.old_size),
        human(d.new_size),
        format_time(old_at),
        format_time(new_at)
    );
    print_diff_children(d, "");
}

fn print_diff_children(d: &DiffNode, indent: &str) {
    let n = d.children.len();
    for (i, c) in d.children.iter().enumerate() {
        let last = i + 1 == n && d.more == 0;
        let (branch, next) = if last {
            ("└─ ", "   ")
        } else {
            ("├─ ", "│  ")
        };
        let name = c
            .path
            .file_name()
            .map(|s| s.to_string_lossy())
            .unwrap_or_default();
        let tag = match c.status {
            "added" => " (new)",
            "removed" => " (deleted)",
            _ => "",
        };
        println!(
            "{indent}{branch}{:>11}  {name}{}{tag}",
            signed(c.delta),
            if c.is_dir { "/" } else { "" }
        );
        print_diff_children(c, &format!("{indent}{next}"));
    }
    if d.more > 0 {
        println!("{indent}└─ ({} more changed)", d.more);
    }
}

pub struct ExportOptions {
    pub dirs_only: bool,
    pub min_size: u64,
    pub mode: SizeMode,
}

/// Rows are collected into one buffer and written in large chunks: an
/// individual `write` per field is the main cost of exporting hundreds of
/// thousands of rows (in JSON also through the `serde_json` formatter).
const EXPORT_CHUNK: usize = 256 * 1024;

struct ChunkedWriter<'a> {
    out: &'a mut dyn Write,
    buf: Vec<u8>,
}

impl<'a> ChunkedWriter<'a> {
    fn new(out: &'a mut dyn Write) -> Self {
        Self {
            out,
            buf: Vec::with_capacity(EXPORT_CHUNK + 1024),
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    fn flush_if_full(&mut self) -> std::io::Result<()> {
        if self.buf.len() >= EXPORT_CHUNK {
            self.out.write_all(&self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }

    fn finish(mut self) -> std::io::Result<()> {
        if !self.buf.is_empty() {
            self.out.write_all(&self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }
}

fn export_rows(
    s: &Scan,
    o: &ExportOptions,
    out: &mut ChunkedWriter<'_>,
    mut row: impl FnMut(&mut Vec<u8>, &Path, &Node),
) -> std::io::Result<()> {
    let mut result = Ok(());
    let mut path = s.root.clone();
    s.tree.walk_into(&mut path, &mut |p, n| {
        if result.is_err() || (o.dirs_only && !n.is_dir()) || n.size_by(o.mode) < o.min_size {
            return;
        }
        row(&mut out.buf, p, n);
        result = out.flush_if_full();
    });
    result
}

fn kind_str(k: NodeKind) -> &'static str {
    match k {
        NodeKind::File => "file",
        NodeKind::Dir => "dir",
        NodeKind::Link => "link",
        NodeKind::Other => "other",
    }
}

fn push_csv_field(buf: &mut Vec<u8>, s: &str) {
    if s.bytes()
        .any(|b| b == b',' || b == b'"' || b == b'\n' || b == b'\r')
    {
        buf.push(b'"');
        for &b in s.as_bytes() {
            if b == b'"' {
                buf.push(b'"');
            }
            buf.push(b);
        }
        buf.push(b'"');
    } else {
        buf.extend_from_slice(s.as_bytes());
    }
}

pub fn export_csv(s: &Scan, o: &ExportOptions, w: &mut dyn Write) -> Result<()> {
    let mut out = ChunkedWriter::new(w);
    out.push(b"path,kind,disk_size,size,file_count,dir_count,modified_utc\n");
    export_rows(s, o, &mut out, |buf, p, n| {
        push_csv_field(buf, &p.to_string_lossy());
        buf.push(b',');
        buf.extend_from_slice(kind_str(n.kind()).as_bytes());
        buf.push(b',');
        push_u64(buf, n.disk_size);
        buf.push(b',');
        push_u64(buf, n.size);
        buf.push(b',');
        push_u64(buf, n.file_count() as u64);
        buf.push(b',');
        push_u64(buf, n.dir_count() as u64);
        buf.push(b',');
        if n.modified().is_some() {
            buf.extend_from_slice(format_time(n.mtime_unix()).as_bytes());
        }
        buf.push(b'\n');
    })?;
    out.finish()?;
    Ok(())
}

pub fn export_json(s: &Scan, o: &ExportOptions, w: &mut dyn Write) -> Result<()> {
    let mut out = ChunkedWriter::new(w);
    out.push(b"[\n");
    let mut first = true;
    export_rows(s, o, &mut out, |buf, p, n| {
        if !first {
            buf.extend_from_slice(b",\n");
        }
        first = false;
        buf.extend_from_slice(b"{\"path\":");
        // Lossy for non-UTF-8 names, so one odd file cannot abort the export.
        push_str(buf, &p.to_string_lossy());
        buf.extend_from_slice(b",\"kind\":\"");
        buf.extend_from_slice(kind_str(n.kind()).as_bytes());
        buf.extend_from_slice(b"\",\"disk_size\":");
        push_u64(buf, n.disk_size);
        buf.extend_from_slice(b",\"size\":");
        push_u64(buf, n.size);
        buf.extend_from_slice(b",\"file_count\":");
        push_u64(buf, n.file_count() as u64);
        buf.extend_from_slice(b",\"dir_count\":");
        push_u64(buf, n.dir_count() as u64);
        buf.extend_from_slice(b",\"modified_utc\":");
        match n.modified() {
            Some(_) => push_str(buf, &format_time(n.mtime_unix())),
            None => buf.extend_from_slice(b"null"),
        }
        buf.push(b'}');
    })?;
    out.push(b"\n]\n");
    out.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cleanmcp_core::scan::{ScanOptions, scan};
    use std::fs;

    /// `"` is illegal in Windows file names, so the quote-doubling case is unix-only.
    fn odd_name() -> &'static str {
        if cfg!(unix) {
            "a,b\"c.txt"
        } else {
            "a,b c.txt"
        }
    }

    fn sample_scan() -> (tempfile::TempDir, Scan) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d");
        fs::create_dir_all(p.join("sub")).unwrap();
        fs::write(p.join(odd_name()), vec![1u8; 300]).unwrap();
        fs::write(p.join("sub").join("file.txt"), vec![1u8; 100]).unwrap();
        let s = scan(&p, &ScanOptions::default()).unwrap();
        (dir, s)
    }

    fn opts() -> ExportOptions {
        ExportOptions {
            dirs_only: false,
            min_size: 0,
            mode: SizeMode::Disk,
        }
    }

    fn file_node(name: &str, size: u64, disk: u64) -> Node {
        let mut n = Node::new(NodeKind::File, name.into(), size, disk, 0);
        n.set_counts(1, 0, 0);
        n
    }

    fn dir_node(name: &str, children: Vec<Node>) -> Node {
        let size = children.iter().map(|c| c.size).sum();
        let disk = children.iter().map(|c| c.disk_size).sum();
        let files = children.iter().map(|c| c.file_count()).sum();
        let dirs = children
            .iter()
            .map(|c| c.dir_count() + u32::from(c.is_dir()))
            .sum();
        let mut n = Node::new(NodeKind::Dir, name.into(), size, disk, 0);
        n.set_children(children);
        n.set_counts(files, dirs, 0);
        n
    }

    #[test]
    fn tree_json_marks_depth_truncation_and_prices_omitted_children() {
        let root = dir_node(
            "root",
            vec![
                dir_node("empty", vec![]),
                dir_node("full", vec![file_node("f", 100, 1_000)]),
            ],
        );
        let path = Path::new("root");
        fn child<'a>(t: &'a TreeJson, name: &str) -> &'a TreeJson {
            t.children
                .iter()
                .find(|c| c.path.ends_with(name))
                .unwrap_or_else(|| panic!("no child {name}"))
        }

        let t = TreeJson::build(&root, path, 1, 10, SizeMode::Apparent);
        assert!(!t.children_truncated, "all children are listed");
        assert!(!child(&t, "empty").children_truncated);
        assert_eq!(child(&t, "empty").other_count, 0);

        // `full` is a folder below the depth limit: not expanded, not empty.
        let full = child(&t, "full");
        assert!(full.children_truncated);
        assert!(full.children.is_empty());
        assert_eq!(full.other_count, 1);
        assert_eq!(full.other_size, 100, "apparent size of the omitted file");
        assert_eq!(full.other_disk_size, 1_000);

        let t = TreeJson::build(&root, path, 1, 10, SizeMode::Disk);
        let full = child(&t, "full");
        assert_eq!(full.other_size, 1_000, "disk size in the default mode");
        assert_eq!(full.other_disk_size, 1_000);
    }

    #[test]
    fn tree_json_marks_a_truncated_tail() {
        let root = dir_node(
            "root",
            vec![
                file_node("a", 300, 3_000),
                file_node("b", 200, 2_000),
                file_node("c", 100, 1_000),
            ],
        );
        let t = TreeJson::build(&root, Path::new("root"), 2, 2, SizeMode::Apparent);
        assert_eq!(t.children.len(), 2);
        assert!(t.children_truncated);
        assert_eq!(t.other_count, 1);
        assert_eq!(t.other_size, 100);
        assert_eq!(t.other_disk_size, 1_000);
        // Files are leaves even when the depth limit stops at them.
        assert!(t.children.iter().all(|c| !c.children_truncated));

        // All children shown: no truncation, no omitted totals.
        let t = TreeJson::build(&root, Path::new("root"), 2, 3, SizeMode::Apparent);
        assert!(!t.children_truncated);
        assert_eq!((t.other_count, t.other_size, t.other_disk_size), (0, 0, 0));
    }

    fn dupe_group(
        name: &str,
        size: u64,
        copies: usize,
        disk: u64,
        wasted_disk: u64,
    ) -> DuplicateGroup {
        let paths = (0..copies)
            .map(|i| PathBuf::from(format!("{name}{i}")))
            .collect();
        DuplicateGroup {
            size,
            disk_size: disk,
            wasted: size * (copies as u64 - 1),
            wasted_disk_size: wasted_disk,
            hash: format!("hash-{name}"),
            paths,
        }
    }

    #[test]
    fn dupes_json_reports_totals_and_prices_by_mode() {
        // Same logical waste, very different on-disk waste: the default (disk)
        // order must put the big allocation first.
        let groups = vec![
            dupe_group("small", 50, 3, 1_000, 2_000),
            dupe_group("big", 100, 2, 10_000, 10_000),
        ];
        let d = DupesJson::build(groups.clone(), 1, SizeMode::Disk);
        assert_eq!(d.total_groups, 2);
        assert_eq!(d.total_wasted, 12_000);
        assert_eq!(d.groups.len(), 1);
        assert_eq!(d.groups[0].paths[0], PathBuf::from("big0"));

        let d = DupesJson::build(groups, 10, SizeMode::Apparent);
        assert_eq!(d.total_groups, 2);
        assert_eq!(d.total_wasted, 200);
        assert_eq!(d.groups.len(), 2);

        let v = serde_json::to_value(DupesJson::build(
            vec![self::dupe_group("g", 10, 2, 4096, 4096)],
            10,
            SizeMode::Apparent,
        ))
        .unwrap();
        let obj = v.as_object().unwrap();
        let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        assert_eq!(keys, ["groups", "total_groups", "total_wasted"]);
        let group = &obj["groups"][0];
        assert_eq!(group["size"], 10);
        assert_eq!(group["disk_size"], 4096);
        assert_eq!(group["wasted"], 10);
        assert_eq!(group["wasted_disk_size"], 4096);
    }

    #[test]
    fn csv_rows_are_escaped_and_complete() {
        let (_dir, s) = sample_scan();
        let mut out = Vec::new();
        export_csv(&s, &opts(), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "path,kind,disk_size,size,file_count,dir_count,modified_utc"
        );
        // header, root, sub, two files
        assert_eq!(lines.len(), 5, "{text}");
        let row = lines
            .iter()
            // CSV doubles an embedded quote, so search for the escaped form.
            .find(|l| l.contains(&odd_name().replace('"', "\"\"")))
            .expect("file row");
        // The whole path is quoted because it contains a comma.
        assert!(row.starts_with('"'), "not CSV-quoted: {row}");
        if cfg!(unix) {
            assert!(row.contains("\"\""), "quote not doubled: {row}");
        }
        let node = s.find(Path::new(odd_name())).unwrap();
        assert!(
            row.ends_with(&format!(
                ",file,{},300,1,0,{}",
                node.disk_size,
                format_time(node.mtime_unix())
            )),
            "{row}"
        );
    }

    /// The old exporter serialized this struct with `serde_json`, one compact
    /// object per line. Comparing against it keeps the key names, their order
    /// and the `null` for unknown times byte-identical.
    #[derive(Serialize)]
    struct OldExportRow {
        path: String,
        kind: String,
        disk_size: u64,
        size: u64,
        file_count: u32,
        dir_count: u32,
        modified_utc: Option<String>,
    }

    #[test]
    fn json_rows_match_old_serde_json_output() {
        let (_dir, s) = sample_scan();
        let mut out = Vec::new();
        export_json(&s, &opts(), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("[\n") && text.ends_with("\n]\n"));
        let mut rows = 0;
        for line in text.lines().filter(|l| l.starts_with('{')) {
            let line = line.trim_end_matches(',');
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            let expected = serde_json::to_string(&OldExportRow {
                path: v["path"].as_str().unwrap().to_string(),
                kind: v["kind"].as_str().unwrap().to_string(),
                disk_size: v["disk_size"].as_u64().unwrap(),
                size: v["size"].as_u64().unwrap(),
                file_count: v["file_count"].as_u64().unwrap() as u32,
                dir_count: v["dir_count"].as_u64().unwrap() as u32,
                modified_utc: v["modified_utc"].as_str().map(str::to_string),
            })
            .unwrap();
            assert_eq!(line, expected);
            assert_eq!(v.as_object().unwrap().len(), 7);
            rows += 1;
        }
        assert_eq!(rows, 4, "{text}");
    }

    #[test]
    fn export_filters_dirs_and_small_entries() {
        let (_dir, s) = sample_scan();
        let o = ExportOptions {
            dirs_only: true,
            min_size: 150,
            mode: SizeMode::Apparent,
        };
        let mut out = Vec::new();
        export_csv(&s, &o, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        // The root (400 bytes) is the only dir at or above 150; `sub` (100) is
        // dropped and files are excluded by `dirs_only`.
        let rows: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(rows.len(), 1, "{text}");
        assert!(rows[0].contains(",dir,"), "{text}");
    }
}
