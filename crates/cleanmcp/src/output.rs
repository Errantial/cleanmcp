//! Human-readable and JSON output for CLI commands.

use std::path::{Path, PathBuf};

use anyhow::Result;
use bytesize::ByteSize;
use cleanmcp_core::query::{Entry, ExtensionStat};
use cleanmcp_core::rules::Candidate;
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
        s.tree.file_count,
        s.tree.dir_count,
        s.elapsed_ms
    );
    print_scan_notes(s);
    print_children(&s.tree, "", depth, top, mode);
}

pub fn print_scan_notes(s: &Scan) {
    let st = &s.stats;
    if s.tree.errors > 0 {
        eprintln!(
            "note: {} entries could not be read (permission denied?)",
            s.tree.errors
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
        let suffix = match c.kind {
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
        let (children, other_disk_size, other_count) = if depth == 0 {
            (Vec::new(), 0, 0)
        } else {
            let sorted = node.sorted_children(mode);
            let kids = sorted
                .iter()
                .take(top)
                .map(|c| TreeJson::build(c, &path.join(&*c.name), depth - 1, top, mode))
                .collect();
            let rest = sorted.get(top..).unwrap_or_default();
            (kids, rest.iter().map(|c| c.disk_size).sum(), rest.len())
        };
        TreeJson {
            path: path.to_path_buf(),
            disk_size: node.disk_size,
            size: node.size,
            is_dir: node.is_dir(),
            file_count: node.file_count as u64,
            age_days: node.age().map(|d| d.as_secs() / 86_400),
            errors: node.errors,
            other_disk_size,
            other_count,
            children,
        }
    }
}
