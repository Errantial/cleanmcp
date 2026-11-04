//! Heuristics for spotting directories that are usually safe to clean up.
//!
//! Rules only *suggest*. Callers must show the user (or agent) what matched
//! and why before anything is removed.

use std::path::Path;

use serde::Serialize;

use crate::query::Entry;
use crate::tree::Node;

#[derive(Debug, Clone, Copy, Serialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Safety {
    /// Regenerated automatically (caches, build output). Deleting costs only rebuild time.
    Regenerable,
    /// Probably unneeded, but may hold user data. Needs a human look.
    Review,
}

#[derive(Debug, Clone, Copy)]
pub struct Rule {
    pub id: &'static str,
    /// Directory name to match (case-insensitive).
    pub dir_name: &'static str,
    /// A sibling that must exist for the rule to match (e.g. `Cargo.toml` next to `target`).
    pub requires_sibling: Option<&'static str>,
    pub safety: Safety,
    pub reason: &'static str,
}

const fn rule(
    id: &'static str,
    dir_name: &'static str,
    requires_sibling: Option<&'static str>,
    safety: Safety,
    reason: &'static str,
) -> Rule {
    Rule {
        id,
        dir_name,
        requires_sibling,
        safety,
        reason,
    }
}

use Safety::{Regenerable, Review};

#[rustfmt::skip]
pub const RULES: &[Rule] = &[
    rule("node_modules", "node_modules", Some("package.json"), Regenerable, "npm/yarn/pnpm dependencies; reinstall with your package manager"),
    rule("cargo_target", "target", Some("Cargo.toml"), Regenerable, "Rust build output; rebuilt by cargo"),
    rule("python_cache", "__pycache__", None, Regenerable, "Python bytecode cache"),
    rule("pytest_cache", ".pytest_cache", None, Regenerable, "pytest cache"),
    rule("python_venv", ".venv", None, Review, "Python virtual environment; recreate from requirements"),
    rule("gradle_build", "build", Some("build.gradle"), Regenerable, "Gradle build output"),
    rule("next_cache", ".next", Some("package.json"), Regenerable, "Next.js build cache"),
    rule("temp", "Temp", None, Review, "Temporary files directory"),
    rule("dot_cache", ".cache", None, Review, "Per-user application caches (Linux/XDG)"),
];

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Candidate {
    pub rule: &'static str,
    pub safety: Safety,
    pub reason: &'static str,
    #[serde(flatten)]
    pub entry: Entry,
}

/// Find directories matching [`RULES`], largest first. Does not descend into a match.
pub fn find_candidates(root: &Node, base: &Path, min_size: u64) -> Vec<Candidate> {
    let mut out = Vec::new();
    visit(root, base, min_size, &mut out);
    out.sort_unstable_by_key(|x| std::cmp::Reverse(x.entry.disk_size));
    out
}

fn visit(node: &Node, path: &Path, min_size: u64, out: &mut Vec<Candidate>) {
    for child in node.children.iter().filter(|c| c.is_dir()) {
        if child.disk_size < min_size {
            continue;
        }
        let child_path = path.join(&*child.name);
        let matched = RULES.iter().find(|r| {
            child.name.eq_ignore_ascii_case(r.dir_name)
                && r.requires_sibling
                    .is_none_or(|s| node.children.iter().any(|c| c.name.eq_ignore_ascii_case(s)))
        });
        match matched {
            Some(rule) => out.push(Candidate {
                rule: rule.id,
                safety: rule.safety,
                reason: rule.reason,
                entry: Entry::from_node(&child_path, child),
            }),
            None => visit(child, &child_path, min_size, out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{ScanOptions, scan};
    use std::fs;

    #[test]
    fn target_needs_cargo_toml() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::create_dir_all(p.join("rust/target")).unwrap();
        fs::write(p.join("rust/Cargo.toml"), "").unwrap();
        fs::write(p.join("rust/target/x"), vec![0u8; 100]).unwrap();
        fs::create_dir_all(p.join("other/target")).unwrap();
        fs::write(p.join("other/target/x"), vec![0u8; 100]).unwrap();

        let s = scan(p, &ScanOptions::default()).unwrap();
        let c = find_candidates(&s.tree, &s.root, 0);
        assert_eq!(c.len(), 1);
        assert!(c[0].entry.path.ends_with("rust/target"));
    }
}
