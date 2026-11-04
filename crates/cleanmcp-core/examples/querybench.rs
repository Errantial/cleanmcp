//! Dev-only harness: time the post-scan queries on a real tree.
//! `cargo run --release --example querybench -- C:\Users\Emil\AppData\Local`
use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use cleanmcp_core::query::{self, FileFilter};
use cleanmcp_core::rules::{self, RuleSet};
use cleanmcp_core::{Node, NodeKind, ScanOptions, SizeMode, scan};

fn best<F: FnMut()>(name: &str, mut f: F) {
    let mut times = Vec::new();
    for _ in 0..3 {
        let t = Instant::now();
        f();
        times.push(t.elapsed().as_secs_f64());
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("{name:<22} min {:.0} ms", times[0] * 1000.0);
}

fn walk(node: &Node, f: &mut impl FnMut(&Node)) {
    if node.kind() == NodeKind::File {
        f(node);
        return;
    }
    for c in node.children().iter() {
        walk(c, f);
    }
}

fn ext_of(name: &str) -> Option<&str> {
    let bytes = name.as_bytes();
    let start = match bytes.iter().rposition(|b| *b == b'/' || *b == b'\\') {
        Some(i) => i + 1,
        None => 0,
    };
    let dot = start + bytes[start..].iter().rposition(|b| *b == b'.')?;
    if dot == start {
        return None;
    }
    Some(&name[dot + 1..])
}

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let t = Instant::now();
    let s = scan(Path::new(&path), &ScanOptions::default()).unwrap();
    let scan_ms = t.elapsed().as_secs_f64() * 1000.0;
    println!("scan                   {scan_ms:.0} ms");
    println!(
        "entries                {} files, {} dirs",
        s.tree.file_count(), s.tree.dir_count()
    );
    best("walk_only", || {
        let mut n = 0u64;
        walk(&s.tree, &mut |x| n += x.disk_size);
        assert!(n > 0);
    });
    best("walk+ext_alloc", || {
        let mut m: HashMap<String, u64> = HashMap::new();
        walk(&s.tree, &mut |x| {
            *m.entry(query::extension(&x.name)).or_default() += 1
        });
        assert!(!m.is_empty());
    });
    best("walk+ext_fast_nomap", || {
        let mut n = 0u64;
        let mut buf = [0u8; 64];
        walk(&s.tree, &mut |x| {
            let e = ext_of(&x.name).unwrap_or("");
            let e = &e[..e.len().min(64)];
            let out = &mut buf[..e.len()];
            out.copy_from_slice(e.as_bytes());
            out.make_ascii_lowercase();
            n += out.len() as u64;
        });
        assert!(n > 0);
    });
    best("walk+ext_fast_map", || {
        // Same work as by_extension but keyed by an FNV hash instead of a String,
        // to separate hashing from the (rare) key allocation.
        let mut m: HashMap<u64, u64> = HashMap::new();
        let mut buf = [0u8; 64];
        walk(&s.tree, &mut |x| {
            let e = ext_of(&x.name).unwrap_or("(none)");
            let e = &e[..e.len().min(64)];
            let out = &mut buf[..e.len()];
            out.copy_from_slice(e.as_bytes());
            out.make_ascii_lowercase();
            let k = std::str::from_utf8(out).unwrap();
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in k.as_bytes() {
                h ^= *b as u64;
                h = h.wrapping_mul(0x1000_0000_01b3);
            }
            match m.get_mut(&h) {
                Some(v) => *v += 1,
                None => {
                    m.insert(h, 1);
                }
            }
        });
        assert!(!m.is_empty());
    });
    best("largest_files(50)", || {
        let v = query::largest_files(&s.tree, &s.root, 50, &FileFilter::default(), SizeMode::Disk);
        assert_eq!(v.len(), 50);
    });
    best("by_extension(20)", || {
        let v = query::by_extension(&s.tree, 20, SizeMode::Disk);
        assert_eq!(v.len(), 20);
    });
    best("by_age", || {
        let v = query::by_age(&s.tree);
        assert_eq!(v.len(), 8);
    });
    let rules = RuleSet::builtin();
    best("find_candidates", || {
        let v = rules::find_candidates(&rules, &s.tree, &s.root, 0);
        assert!(!v.is_empty());
    });
}
