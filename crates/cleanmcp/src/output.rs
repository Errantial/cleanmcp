//! Human-readable and JSON output for CLI commands.

use std::path::{Path, PathBuf};

use anyhow::Result;
use bytesize::ByteSize;
use cleanmcp_core::query::{Entry, ExtensionStat};
use cleanmcp_core::rules::Candidate;
use cleanmcp_core::{Node, Scan};
use serde::Serialize;

pub fn human(bytes: u64) -> String {
    ByteSize(bytes).display().iec().to_string()
}

fn pct(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 * 100.0 / total as f64
    }
}

pub fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

pub fn print_tree(s: &Scan, depth: usize, top: usize) {
    println!(
        "{}  {}  ({} files, {} dirs, scanned in {} ms)",
        human(s.tree.size),
        s.root.display(),
        s.tree.file_count,
        s.tree.dir_count,
        s.elapsed_ms
    );
    if s.tree.errors > 0 {
        println!("  ! {} entries could not be read", s.tree.errors);
    }
    print_children(&s.tree, "", depth, top);
}

fn print_children(node: &Node, indent: &str, depth: usize, top: usize) {
    if depth == 0 {
        return;
    }
    let shown = node.children.len().min(top);
    for (i, c) in node.children.iter().take(top).enumerate() {
        let last = i + 1 == shown && node.children.len() <= top;
        let (branch, next) = if last {
            ("└─ ", "   ")
        } else {
            ("├─ ", "│  ")
        };
        let slash = if c.is_dir() { "/" } else { "" };
        println!(
            "{indent}{branch}{:>10} {:>5.1}%  {}{slash}",
            human(c.size),
            pct(c.size, node.size),
            c.name
        );
        if c.is_dir() {
            print_children(c, &format!("{indent}{next}"), depth - 1, top);
        }
    }
    if node.children.len() > top {
        let rest: u64 = node.children[top..].iter().map(|c| c.size).sum();
        println!(
            "{indent}└─ {:>10}         ({} more)",
            human(rest),
            node.children.len() - top
        );
    }
}

pub fn print_entries(entries: &[Entry]) {
    for e in entries {
        let age = e
            .age_secs
            .map(|s| format!("{}d", s / 86_400))
            .unwrap_or_default();
        println!("{:>10}  {:>6}  {}", human(e.size), age, e.path.display());
    }
}

pub fn print_extensions(stats: &[ExtensionStat], total: u64) {
    for s in stats {
        println!(
            "{:>10} {:>5.1}%  {:>8} files  .{}",
            human(s.size),
            pct(s.size, total),
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
    let total: u64 = c.iter().map(|c| c.entry.size).sum();
    for c in c {
        println!(
            "{:>10}  {:<11}  {}\n            {}",
            human(c.entry.size),
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
    pub size: u64,
    pub is_dir: bool,
    pub file_count: u64,
    pub errors: u64,
    /// Children beyond the `top` limit are summed into `other_size`.
    pub other_size: u64,
    pub other_count: usize,
    pub children: Vec<TreeJson>,
}

impl TreeJson {
    pub fn build(node: &Node, path: &Path, depth: usize, top: usize) -> Self {
        let children = if depth == 0 {
            Vec::new()
        } else {
            node.children
                .iter()
                .take(top)
                .map(|c| TreeJson::build(c, &path.join(&c.name), depth - 1, top))
                .collect()
        };
        let (other_size, other_count) = if depth == 0 || node.children.len() <= top {
            (0, 0)
        } else {
            (
                node.children[top..].iter().map(|c| c.size).sum(),
                node.children.len() - top,
            )
        };
        TreeJson {
            path: path.to_path_buf(),
            size: node.size,
            is_dir: node.is_dir(),
            file_count: node.file_count,
            errors: node.errors,
            other_size,
            other_count,
            children,
        }
    }
}
