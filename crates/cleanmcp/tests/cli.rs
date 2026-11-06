//! End-to-end: run the real binary against a fixture tree and check the output.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const EXE: &str = env!("CARGO_BIN_EXE_cleanmcp");

/// A small tree with a `.gitignore`, used by every test here.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    fs::create_dir_all(p.join("src")).unwrap();
    fs::create_dir_all(p.join("target/debug")).unwrap();
    fs::write(p.join(".gitignore"), "target/\n*.log\n").unwrap();
    // Big enough to be the largest file even after cluster rounding on any OS.
    fs::write(p.join("src/main.rs"), vec![1u8; 100_000]).unwrap();
    fs::write(p.join("src/lib.rs"), vec![1u8; 500]).unwrap();
    fs::write(p.join("target/debug/app"), vec![1u8; 100_000]).unwrap();
    fs::write(p.join("notes.log"), vec![1u8; 50]).unwrap();
    dir
}

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(EXE)
        .args(args)
        .arg(dir.as_os_str())
        // Keep the user's real config out of tests.
        .env("CLEANMCP_CONFIG", dir.join("none.toml"))
        .output()
        .unwrap()
}

fn run_json(dir: &Path, args: &[&str]) -> Value {
    let mut full = args.to_vec();
    full.push("--json");
    let out = run(dir, &full);
    assert!(
        out.status.success(),
        "command failed: {:?}\n{}",
        full,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn tree_json_has_totals_and_errors_field() {
    let dir = fixture();
    let v = run_json(dir.path(), &["tree", "-d", "2", "-q"]);
    assert_eq!(v["errors"], 0);
    assert_eq!(v["file_count"], 3); // .gitignore, main.rs, lib.rs (target/ and *.log ignored)
    assert!(v["disk_size"].as_u64().unwrap() >= 1_500);
    assert!(v["children"].is_array());
}

#[test]
fn no_ignore_flag_scans_ignored_entries() {
    let dir = fixture();
    let v = run_json(dir.path(), &["tree", "-d", "3", "-q", "--no-ignore"]);
    assert_eq!(v["file_count"], 5); // everything, including target/debug/app and notes.log
}

#[test]
fn exclude_still_applies_on_top_of_gitignore() {
    let dir = fixture();
    let v = run_json(dir.path(), &["tree", "-d", "2", "-q", "-e", "src"]);
    assert_eq!(v["file_count"], 1); // only .gitignore
}

#[test]
fn files_ext_age_suggest_json() {
    let dir = fixture();
    let files = run_json(dir.path(), &["files", "-n", "10", "-q"]);
    assert_eq!(files.as_array().unwrap().len(), 3);
    assert!(files[0]["path"].as_str().unwrap().ends_with("main.rs"));

    let ext = run_json(dir.path(), &["ext", "-q"]);
    let exts: Vec<&str> = ext
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["extension"].as_str().unwrap())
        .collect();
    assert!(exts.contains(&"rs"));
    assert!(!exts.contains(&"log"));

    let age = run_json(dir.path(), &["age", "-q"]);
    let total: u64 = age
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["file_count"].as_u64().unwrap())
        .sum();
    assert_eq!(total, 3);

    let suggest = run_json(dir.path(), &["suggest", "--min-size", "1", "-q"]);
    assert!(suggest["candidates"].is_array());
}

#[test]
fn du_prints_size_tab_path_per_directory() {
    let dir = fixture();
    let out = run(dir.path(), &["du", "-q"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().collect();
    // root + src (target/ is ignored)
    assert_eq!(lines.len(), 2, "unexpected du output:\n{text}");
    for line in &lines {
        let (size, path) = line.split_once('\t').expect("size<TAB>path format");
        assert!(size.parse::<u64>().is_ok(), "size is a number: {line}");
        assert!(Path::new(path).is_absolute() || path == ".");
    }
    // The scan root is resolved (canonicalized), so compare canonical paths.
    let root_line = lines[0].split_once('\t').unwrap().1;
    assert_eq!(
        std::fs::canonicalize(root_line).unwrap(),
        std::fs::canonicalize(dir.path()).unwrap()
    );

    // -a also lists files.
    let out = run(dir.path(), &["du", "-a", "-q"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.lines().count() >= 5, "du -a lists files too:\n{text}");

    // --json is a structured list.
    let v = run_json(dir.path(), &["du", "-q"]);
    assert_eq!(v["entries"].as_array().unwrap().len(), 2);
    assert_eq!(v["size_mode"], "disk");
}

#[test]
fn snapshot_then_diff_round_trips() {
    let dir = fixture();
    let snap = dir.path().join("snap.jsonl");
    let out = Command::new(EXE)
        .args(["snapshot", "-q", "-o"])
        .arg(&snap)
        .arg(dir.path())
        .env("CLEANMCP_CONFIG", dir.path().join("none.toml"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(snap.exists());

    // `diff OLD` rescans the snapshot's root and compares.
    let out = Command::new(EXE)
        .args(["diff", "-q", "--json"])
        .arg(&snap)
        .env("CLEANMCP_CONFIG", dir.path().join("none.toml"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["path"].is_string());
    assert!(v["delta"].is_number());
}

#[test]
fn bad_exclude_pattern_is_a_clear_error() {
    let dir = fixture();
    let out = run(dir.path(), &["tree", "-q", "-e", "[invalid"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("invalid --exclude pattern"), "{err}");
}
