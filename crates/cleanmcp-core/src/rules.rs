//! Heuristics for spotting directories that are usually safe to clean up.
//!
//! Rules only *suggest*. Callers must show the user (or agent) what matched
//! and why before anything is removed. Users can add rules and disable
//! built-in ones in the config file (see [`crate::config`]).

use std::path::Path;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::query::Entry;
use crate::tree::Node;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Safety {
    /// Regenerated automatically (caches, build output). Deleting costs only rebuild/download time.
    Regenerable,
    /// Probably unneeded, but may hold data someone wants. Needs a human look.
    Review,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Os {
    #[default]
    Any,
    Windows,
    Linux,
}

impl Os {
    fn applies(self) -> bool {
        match self {
            Os::Any => true,
            Os::Windows => cfg!(windows),
            Os::Linux => cfg!(target_os = "linux"),
        }
    }
}

/// One cleanup rule. Exactly one of `dir_name` / `path` should be set.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    /// Match directories with this name (case-insensitive), anywhere.
    #[serde(default)]
    pub dir_name: Option<String>,
    /// Match directories whose full path matches this glob (`/` separators,
    /// case-insensitive), e.g. `**/AppData/Local/Temp`.
    #[serde(default)]
    pub path: Option<String>,
    /// Only match if the parent directory also contains one of these names
    /// (`*.ext` allowed), e.g. `Cargo.toml` next to `target`.
    #[serde(default)]
    pub requires_sibling: Vec<String>,
    /// Only match if the directory itself contains one of these names.
    #[serde(default)]
    pub requires_child: Vec<String>,
    pub safety: Safety,
    pub reason: String,
    /// How to clean it properly, if better than deleting the folder (e.g. a command).
    #[serde(default)]
    pub advice: Option<String>,
    #[serde(default)]
    pub os: Os,
}

/// Compact form for the built-in table.
#[derive(Clone, Copy)]
struct B {
    id: &'static str,
    name: Option<&'static str>,
    path: Option<&'static str>,
    sibling: &'static [&'static str],
    child: &'static [&'static str],
    safety: Safety,
    reason: &'static str,
    advice: Option<&'static str>,
    os: Os,
}

const fn n(
    id: &'static str,
    name: &'static str,
    sibling: &'static [&'static str],
    safety: Safety,
    reason: &'static str,
) -> B {
    B {
        id,
        name: Some(name),
        path: None,
        sibling,
        child: &[],
        safety,
        reason,
        advice: None,
        os: Os::Any,
    }
}

const fn p(
    id: &'static str,
    path: &'static str,
    os: Os,
    safety: Safety,
    reason: &'static str,
    advice: Option<&'static str>,
) -> B {
    B {
        id,
        name: None,
        path: Some(path),
        sibling: &[],
        child: &[],
        safety,
        reason,
        advice,
        os,
    }
}

use Os::{Any, Linux, Windows};
use Safety::{Regenerable as Regen, Review};

/// Built-in rules. Order matters: the first matching rule wins, and a matched
/// directory is not searched further, so specific rules come before generic ones.
#[rustfmt::skip]
const BUILTIN: &[B] = &[
    // --- Project build output and dependencies -------------------------------
    n("node_modules", "node_modules", &["package.json"], Regen, "npm/yarn/pnpm dependencies; reinstall with your package manager"),
    n("cargo_target", "target", &["Cargo.toml"], Regen, "Rust build output; rebuilt by cargo"),
    n("gradle_build", "build", &["build.gradle", "build.gradle.kts"], Regen, "Gradle build output"),
    n("gradle_project_cache", ".gradle", &["settings.gradle", "settings.gradle.kts", "build.gradle", "build.gradle.kts"], Regen, "Gradle per-project cache"),
    n("maven_target", "target", &["pom.xml"], Regen, "Maven build output"),
    n("dotnet_bin", "bin", &["*.csproj", "*.fsproj", "*.vbproj"], Regen, ".NET build output"),
    n("dotnet_obj", "obj", &["*.csproj", "*.fsproj", "*.vbproj"], Regen, ".NET intermediate build files"),
    n("next_cache", ".next", &["package.json"], Regen, "Next.js build output"),
    n("nuxt_cache", ".nuxt", &["package.json"], Regen, "Nuxt build output"),
    n("svelte_kit", ".svelte-kit", &["package.json"], Regen, "SvelteKit build output"),
    n("turbo_cache", ".turbo", &[], Regen, "Turborepo cache"),
    n("parcel_cache", ".parcel-cache", &[], Regen, "Parcel bundler cache"),
    n("angular_cache", ".angular", &["angular.json"], Regen, "Angular CLI cache"),
    n("python_cache", "__pycache__", &[], Regen, "Python bytecode cache"),
    n("pytest_cache", ".pytest_cache", &[], Regen, "pytest cache"),
    n("mypy_cache", ".mypy_cache", &[], Regen, "mypy cache"),
    n("ruff_cache", ".ruff_cache", &[], Regen, "Ruff linter cache"),
    n("tox", ".tox", &["tox.ini", "pyproject.toml", "setup.py"], Regen, "tox test environments"),
    B { child: &["pyvenv.cfg"], ..n("python_venv", ".venv", &[], Review, "Python virtual environment; recreate from requirements/pyproject") },
    B { child: &["pyvenv.cfg"], ..n("python_venv2", "venv", &[], Review, "Python virtual environment; recreate from requirements/pyproject") },
    n("terraform", ".terraform", &[], Regen, "Terraform providers/modules; restored by `terraform init`"),
    n("dart_tool", ".dart_tool", &["pubspec.yaml"], Regen, "Dart/Flutter tool cache"),
    n("cocoapods", "Pods", &["Podfile"], Regen, "CocoaPods dependencies; reinstall with `pod install`"),
    n("zig_cache", ".zig-cache", &[], Regen, "Zig build cache"),
    n("zig_cache_old", "zig-cache", &[], Regen, "Zig build cache"),

    // --- Windows --------------------------------------------------------------
    p("win_user_temp", "**/AppData/Local/Temp", Windows, Review, "Your temporary files; files not in use can be deleted", Some("Settings > System > Storage > Temporary files")),
    p("win_temp", "*:/Windows/Temp", Windows, Review, "Windows temporary files", Some("Run Disk Cleanup (cleanmgr) as administrator")),
    p("win_update_cache", "*:/Windows/SoftwareDistribution/Download", Windows, Regen, "Downloaded Windows Update files", Some("Disk Cleanup > Clean up system files > Windows Update Cleanup")),
    p("win_old", "*:/Windows.old", Windows, Review, "Previous Windows installation (needed only to roll back an upgrade)", Some("Disk Cleanup > Clean up system files > Previous Windows installation(s)")),
    p("win_recycle_bin", "*:/$Recycle.Bin", Windows, Review, "Recycle Bin contents", Some("Empty the Recycle Bin")),
    p("win_crash_dumps", "**/AppData/Local/CrashDumps", Windows, Regen, "Application crash dumps", None),
    p("win_minidump", "*:/Windows/Minidump", Windows, Review, "Windows crash dumps", Some("Disk Cleanup > System error memory dump files")),
    p("win_inet_cache", "**/AppData/Local/Microsoft/Windows/INetCache", Windows, Regen, "Internet/WebView cache", None),
    p("win_thumbcache", "**/AppData/Local/Microsoft/Windows/Explorer", Windows, Regen, "Explorer thumbnail and icon cache", Some("Disk Cleanup > Thumbnails")),
    p("chrome_cache", "**/AppData/Local/Google/Chrome/User Data/*/Cache", Windows, Regen, "Chrome browser cache", None),
    p("chrome_code_cache", "**/AppData/Local/Google/Chrome/User Data/*/Code Cache", Windows, Regen, "Chrome compiled script cache", None),
    p("edge_cache", "**/AppData/Local/Microsoft/Edge/User Data/*/Cache", Windows, Regen, "Edge browser cache", None),
    p("edge_code_cache", "**/AppData/Local/Microsoft/Edge/User Data/*/Code Cache", Windows, Regen, "Edge compiled script cache", None),
    p("firefox_cache", "**/AppData/Local/Mozilla/Firefox/Profiles/*/cache2", Windows, Regen, "Firefox browser cache", None),
    p("npm_cache_win", "**/AppData/Local/npm-cache", Windows, Regen, "npm download cache", Some("npm cache clean --force")),
    p("pnpm_store_win", "**/AppData/Local/pnpm/store", Windows, Regen, "pnpm package store", Some("pnpm store prune")),
    p("yarn_cache_win", "**/AppData/Local/Yarn/Cache", Windows, Regen, "Yarn download cache", Some("yarn cache clean")),
    p("pip_cache_win", "**/AppData/Local/pip/Cache", Windows, Regen, "pip download cache", Some("pip cache purge")),
    p("nuget_http_cache", "**/AppData/Local/NuGet/v3-cache", Windows, Regen, "NuGet HTTP cache", Some("dotnet nuget locals http-cache --clear")),
    p("go_build_cache_win", "**/AppData/Local/go-build", Windows, Regen, "Go build cache", Some("go clean -cache")),
    p("docker_wsl", "**/AppData/Local/Docker/wsl", Windows, Review, "Docker Desktop virtual disk (images, containers, volumes)", Some("docker system prune, then compact the .vhdx")),
    p("vscode_cache", "**/AppData/Roaming/Code/Cache*", Windows, Regen, "VS Code cache", None),

    // --- Linux ----------------------------------------------------------------
    p("journal", "/var/log/journal", Linux, Review, "systemd journal logs", Some("sudo journalctl --vacuum-size=200M")),
    p("apt_cache", "/var/cache/apt/archives", Linux, Regen, "Downloaded .deb packages", Some("sudo apt clean")),
    p("dnf_cache", "/var/cache/dnf", Linux, Regen, "dnf package cache", Some("sudo dnf clean all")),
    p("pacman_cache", "/var/cache/pacman/pkg", Linux, Regen, "pacman package cache", Some("sudo paccache -r")),
    p("snap_revisions", "/var/lib/snapd/snaps", Linux, Review, "Snap packages including old revisions", Some("Remove disabled revisions: snap list --all, then snap remove --revision")),
    p("docker_data", "/var/lib/docker", Linux, Review, "Docker images, containers and volumes", Some("docker system prune (add -a and --volumes to remove more)")),
    p("trash", "**/.local/share/Trash", Linux, Review, "Desktop trash can", Some("Empty the trash")),
    p("pip_cache", "**/.cache/pip", Linux, Regen, "pip download cache", Some("pip cache purge")),
    p("go_build_cache", "**/.cache/go-build", Linux, Regen, "Go build cache", Some("go clean -cache")),
    p("yarn_cache", "**/.cache/yarn", Linux, Regen, "Yarn download cache", Some("yarn cache clean")),
    p("chrome_cache_linux", "**/.cache/google-chrome", Linux, Regen, "Chrome browser cache", None),
    p("chromium_cache_linux", "**/.cache/chromium", Linux, Regen, "Chromium browser cache", None),
    p("firefox_cache_linux", "**/.cache/mozilla", Linux, Regen, "Firefox browser cache", None),
    p("thumbnails", "**/.cache/thumbnails", Linux, Regen, "Thumbnail cache", None),
    p("pnpm_store", "**/.local/share/pnpm/store", Linux, Regen, "pnpm package store", Some("pnpm store prune")),

    // --- Both: per-user developer and AI caches -------------------------------
    p("huggingface_cache", "**/.cache/huggingface", Any, Review, "Downloaded Hugging Face models and datasets", Some("huggingface-cli delete-cache")),
    p("ollama_models", "**/.ollama/models", Any, Review, "Ollama models", Some("ollama list, then ollama rm <model>")),
    p("npm_cache", "**/.npm/_cacache", Any, Regen, "npm download cache", Some("npm cache clean --force")),
    p("cargo_registry_cache", "**/.cargo/registry/cache", Any, Regen, "Downloaded crate archives", Some("cargo install cargo-cache && cargo cache --autoclean")),
    p("cargo_registry_src", "**/.cargo/registry/src", Any, Regen, "Extracted crate sources", Some("cargo cache --autoclean")),
    p("cargo_git", "**/.cargo/git/checkouts", Any, Regen, "Git dependency checkouts", Some("cargo cache --autoclean")),
    p("gradle_caches", "**/.gradle/caches", Any, Regen, "Gradle dependency cache", None),
    p("maven_repo", "**/.m2/repository", Any, Review, "Maven local repository (re-downloaded on demand)", None),
    p("go_mod_cache", "**/go/pkg/mod", Any, Review, "Go module cache", Some("go clean -modcache")),
    p("nuget_packages", "**/.nuget/packages", Any, Review, "NuGet package cache", Some("dotnet nuget locals global-packages --clear")),
    p("bun_cache", "**/.bun/install/cache", Any, Regen, "Bun package cache", Some("bun pm cache rm")),
    p("conda_pkgs", "**/miniforge3/pkgs", Any, Regen, "Conda package cache", Some("conda clean --all")),
    p("conda_pkgs2", "**/miniconda3/pkgs", Any, Regen, "Conda package cache", Some("conda clean --all")),
    p("conda_pkgs3", "**/anaconda3/pkgs", Any, Regen, "Conda package cache", Some("conda clean --all")),
    // Generic fallback: anything else in ~/.cache, one entry per application.
    p("xdg_cache", "**/.cache/*", Linux, Review, "Application cache", None),
];

impl From<&B> for Rule {
    fn from(b: &B) -> Self {
        Rule {
            id: b.id.into(),
            dir_name: b.name.map(Into::into),
            path: b.path.map(Into::into),
            requires_sibling: b.sibling.iter().map(|s| s.to_string()).collect(),
            requires_child: b.child.iter().map(|s| s.to_string()).collect(),
            safety: b.safety,
            reason: b.reason.into(),
            advice: b.advice.map(Into::into),
            os: b.os,
        }
    }
}

/// Built-in rules (all platforms; [`RuleSet::new`] drops the ones for other OSes).
pub fn builtin_rules() -> Vec<Rule> {
    BUILTIN.iter().map(Rule::from).collect()
}

/// Compiled rule set ready for matching.
pub struct RuleSet {
    rules: Vec<Rule>,
    /// Globs for the rules that have a `path`; `path_rule[i]` is the rule index of glob `i`.
    paths: GlobSet,
    path_rule: Vec<usize>,
}

impl RuleSet {
    /// Compile `rules`. Rules for other operating systems are dropped.
    pub fn new(rules: Vec<Rule>) -> Result<Self, globset::Error> {
        let rules: Vec<Rule> = rules.into_iter().filter(|r| r.os.applies()).collect();
        let mut b = GlobSetBuilder::new();
        let mut path_rule = Vec::new();
        for (i, r) in rules.iter().enumerate() {
            if let Some(p) = &r.path {
                let p = p.replace('\\', "/");
                b.add(
                    GlobBuilder::new(&p)
                        .case_insensitive(true)
                        .literal_separator(true)
                        .build()?,
                );
                path_rule.push(i);
            }
        }
        Ok(RuleSet {
            paths: b.build()?,
            rules,
            path_rule,
        })
    }

    /// Built-in rules only.
    pub fn builtin() -> Self {
        Self::new(builtin_rules()).expect("built-in rules are valid")
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    fn matching(&self, parent: &Node, dir: &Node, path: &Path) -> Option<&Rule> {
        self.matching_with(&dir.name, dir, path, |s| has_child(parent, s))
    }

    /// Like `matching`, for a directory whose parent is not in the tree (the scan
    /// root): sibling requirements are checked on disk.
    fn matching_root(&self, dir: &Node, path: &Path) -> Option<&Rule> {
        let siblings: Vec<String> = path
            .parent()
            .and_then(|p| std::fs::read_dir(p).ok())
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        // The root node is named by its full path; match on the folder name.
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default();
        self.matching_with(&name, dir, path, |s| {
            siblings.iter().any(|n| name_matches(n, s))
        })
    }

    fn matching_with(
        &self,
        name: &str,
        dir: &Node,
        path: &Path,
        has_sibling: impl Fn(&str) -> bool,
    ) -> Option<&Rule> {
        let mut path_hits: Option<Vec<usize>> = None;
        self.rules.iter().enumerate().find_map(|(i, r)| {
            let hit = match (&r.dir_name, &r.path) {
                (Some(rule_name), _) => name.eq_ignore_ascii_case(rule_name),
                (None, Some(_)) => path_hits
                    .get_or_insert_with(|| self.paths.matches(path))
                    .iter()
                    .any(|&h| self.path_rule[h] == i),
                _ => false,
            };
            let ok = hit
                && (r.requires_sibling.is_empty()
                    || r.requires_sibling.iter().any(|s| has_sibling(s)))
                && (r.requires_child.is_empty()
                    || r.requires_child.iter().any(|s| has_child(dir, s)));
            ok.then_some(r)
        })
    }
}

/// Does `dir` directly contain an entry called `pattern` (`*.ext` allowed)?
fn has_child(dir: &Node, pattern: &str) -> bool {
    dir.children()
        .iter()
        .any(|c| name_matches(&c.name, pattern))
}

/// Case-insensitive name match; `*.ext` matches any name ending in `.ext`.
fn name_matches(name: &str, pattern: &str) -> bool {
    match pattern.strip_prefix('*') {
        Some(suffix) => {
            name.len() > suffix.len()
                && name.is_char_boundary(name.len() - suffix.len())
                && name[name.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
        }
        None => name.eq_ignore_ascii_case(pattern),
    }
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Candidate {
    pub rule: String,
    pub safety: Safety,
    pub reason: String,
    /// A better way to clean this than deleting the folder, if there is one.
    pub advice: Option<String>,
    #[serde(flatten)]
    pub entry: Entry,
}

/// Find directories matching `rules`, largest (on disk) first. Does not descend into a match.
pub fn find_candidates(rules: &RuleSet, root: &Node, base: &Path, min_size: u64) -> Vec<Candidate> {
    let mut out = Vec::new();
    // The scan root itself can match too (e.g. scanning ~/.npm/_cacache directly).
    match rules.matching_root(root, base) {
        Some(r) if root.is_dir() => {
            if root.disk_size >= min_size {
                out.push(candidate(r, base, root));
            }
        }
        _ => visit(rules, root, base, min_size, &mut out),
    }
    out.sort_unstable_by_key(|c| std::cmp::Reverse(c.entry.disk_size));
    out
}

fn candidate(r: &Rule, path: &Path, n: &Node) -> Candidate {
    Candidate {
        rule: r.id.clone(),
        safety: r.safety,
        reason: r.reason.clone(),
        advice: r.advice.clone(),
        entry: Entry::from_node(path, n),
    }
}

fn visit(rules: &RuleSet, node: &Node, path: &Path, min_size: u64, out: &mut Vec<Candidate>) {
    for child in node.children().iter() {
        // Children are sorted by disk size, so everything after this is smaller too.
        if child.disk_size < min_size {
            break;
        }
        if !child.is_dir() {
            continue;
        }
        let child_path = path.join(child.name.as_str());
        match rules.matching(node, child, &child_path) {
            Some(rule) => out.push(candidate(rule, &child_path, child)),
            None => visit(rules, child, &child_path, min_size, out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{ScanOptions, scan};
    use std::fs;

    fn rule_ids(p: &Path, rules: &RuleSet) -> Vec<String> {
        let s = scan(p, &ScanOptions::default()).unwrap();
        let mut ids: Vec<String> = find_candidates(rules, &s.tree, &s.root, 0)
            .into_iter()
            .map(|c| c.rule)
            .collect();
        ids.sort();
        ids
    }

    #[test]
    fn target_needs_cargo_toml() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::create_dir_all(p.join("rust/target")).unwrap();
        fs::write(p.join("rust/Cargo.toml"), "").unwrap();
        fs::write(p.join("rust/target/x"), vec![1u8; 100]).unwrap();
        fs::create_dir_all(p.join("other/target")).unwrap();
        fs::write(p.join("other/target/x"), vec![1u8; 100]).unwrap();
        assert_eq!(rule_ids(p, &RuleSet::builtin()), ["cargo_target"]);
    }

    #[test]
    fn scan_root_checks_real_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        // A project *named* target containing Cargo.toml is not build output.
        fs::create_dir_all(p.join("target/src")).unwrap();
        fs::write(p.join("target/Cargo.toml"), "").unwrap();
        assert!(rule_ids(&p.join("target"), &RuleSet::builtin()).is_empty());
        // Scanning proj/target directly does match (Cargo.toml is next to it).
        fs::create_dir_all(p.join("proj/target")).unwrap();
        fs::write(p.join("proj/Cargo.toml"), "").unwrap();
        fs::write(p.join("proj/target/x"), vec![1u8; 10]).unwrap();
        assert_eq!(
            rule_ids(&p.join("proj/target"), &RuleSet::builtin()),
            ["cargo_target"]
        );
    }

    #[test]
    fn wildcard_sibling_and_required_child() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::create_dir_all(p.join("app/bin")).unwrap();
        fs::write(p.join("app/App.csproj"), "").unwrap();
        fs::write(p.join("app/bin/a.dll"), vec![1u8; 10]).unwrap();
        fs::create_dir_all(p.join("py/.venv")).unwrap();
        fs::write(p.join("py/.venv/pyvenv.cfg"), "").unwrap();
        fs::create_dir_all(p.join("notvenv/.venv")).unwrap();
        fs::write(p.join("notvenv/.venv/x"), "").unwrap();
        assert_eq!(
            rule_ids(p, &RuleSet::builtin()),
            ["dotnet_bin", "python_venv"]
        );
    }

    #[test]
    fn path_rules_and_user_rules() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::create_dir_all(p.join("home/.npm/_cacache")).unwrap();
        fs::write(p.join("home/.npm/_cacache/x"), vec![1u8; 10]).unwrap();
        fs::create_dir_all(p.join("home/stuff/renders")).unwrap();
        fs::write(p.join("home/stuff/renders/x"), vec![1u8; 10]).unwrap();

        let mut rules = builtin_rules();
        rules.push(Rule {
            id: "my_renders".into(),
            dir_name: None,
            path: Some("**/stuff/renders".into()),
            requires_sibling: vec![],
            requires_child: vec![],
            safety: Safety::Review,
            reason: "old renders".into(),
            advice: None,
            os: Os::Any,
        });
        assert_eq!(
            rule_ids(p, &RuleSet::new(rules).unwrap()),
            ["my_renders", "npm_cache"]
        );
    }
}
