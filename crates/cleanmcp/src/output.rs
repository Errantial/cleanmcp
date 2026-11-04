//! Human-readable and JSON output for CLI commands.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use bytesize::ByteSize;
use cleanmcp_core::drives::Drive;
use cleanmcp_core::dupes::DuplicateGroup;
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

pub fn print_candidates(c: &[Candidate]) {
    if c.is_empty() {
        println!("No cleanup candidates found.");
        return;
    }
    let total: u64 = c.iter().map(|c| c.entry.disk_size).sum();
    for c in c {
        println!(
            "{:>10}  {:<11}  {}\n            {}",
            human(c.entry.disk_size),
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
    /// Children beyond the `top` limit are summed into `other_disk_size`.
    pub other_disk_size: u64,
    pub other_count: usize,
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
        let (children, other_disk_size, other_count) = if depth == 0 {
            (Vec::new(), 0, 0)
        } else {
            let sorted = node.sorted_children(mode);
            let start = offset.min(sorted.len());
            let kids = sorted[start..]
                .iter()
                .take(top)
                .map(|c| TreeJson::build(c, &path.join(c.name.as_str()), depth - 1, top, mode))
                .collect();
            let rest = sorted.get(start + top..).unwrap_or_default();
            (kids, rest.iter().map(|c| c.disk_size).sum(), rest.len())
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
            other_count,
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

pub fn print_dupes(groups: &[DuplicateGroup], total_groups: usize, total_wasted: u64) {
    if groups.is_empty() {
        println!("No duplicate files found.");
        return;
    }
    for g in groups {
        println!(
            "{} wasted  ({} copies of {})",
            human(g.wasted),
            g.paths.len(),
            human(g.size)
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

fn export_rows(
    s: &Scan,
    o: &ExportOptions,
    mut f: impl FnMut(&Path, &Node) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut result = Ok(());
    s.tree.walk(&s.root, &mut |p, n| {
        if result.is_err() || (o.dirs_only && !n.is_dir()) || n.size_by(o.mode) < o.min_size {
            return;
        }
        result = f(p, n);
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

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

pub fn export_csv(s: &Scan, o: &ExportOptions, w: &mut dyn Write) -> Result<()> {
    writeln!(
        w,
        "path,kind,disk_size,size,file_count,dir_count,modified_utc"
    )?;
    export_rows(s, o, |p, n| {
        writeln!(
            w,
            "{},{},{},{},{},{},{}",
            csv_field(&p.to_string_lossy()),
            kind_str(n.kind()),
            n.disk_size,
            n.size,
            n.file_count(),
            n.dir_count(),
            n.modified()
                .map(|_| format_time(n.mtime_unix()))
                .unwrap_or_default()
        )
    })?;
    Ok(())
}

#[derive(Serialize)]
struct ExportRow {
    /// Lossy for non-UTF-8 names, so one odd file cannot abort the export.
    path: String,
    kind: &'static str,
    disk_size: u64,
    size: u64,
    file_count: u32,
    dir_count: u32,
    modified_utc: Option<String>,
}

pub fn export_json(s: &Scan, o: &ExportOptions, w: &mut dyn Write) -> Result<()> {
    w.write_all(b"[\n")?;
    let mut first = true;
    export_rows(s, o, |p, n| {
        if !first {
            w.write_all(b",\n")?;
        }
        first = false;
        let row = ExportRow {
            path: p.to_string_lossy().into_owned(),
            kind: kind_str(n.kind()),
            disk_size: n.disk_size,
            size: n.size,
            file_count: n.file_count(),
            dir_count: n.dir_count(),
            modified_utc: n.modified().map(|_| format_time(n.mtime_unix())),
        };
        serde_json::to_writer(&mut *w, &row).map_err(std::io::Error::other)
    })?;
    w.write_all(b"\n]\n")?;
    Ok(())
}
