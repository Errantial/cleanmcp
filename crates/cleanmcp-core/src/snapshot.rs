//! Save a scan to disk and compare two scans to see what grew.
//!
//! Format: JSON Lines. Line 1 is a header object; every other line is one node
//! in depth-first order: `[depth, kind, name, size, disk_size, mtime, errors]`.
//! A flat format keeps memory low and avoids JSON nesting limits on deep trees.
//! Files ending in `.gz` are gzip-compressed.
//!
//! Rows are written by hand into a large buffer and parsed by a small dedicated
//! parser on load: at a few hundred thousand rows that is several times faster
//! than generic JSON serialization. The format itself is unchanged (version 1)
//! and the rows are byte-identical to what `serde_json` produced in 0.1.0, so
//! old snapshots still load and old binaries can read new snapshots.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use compact_str::CompactString;
use flate2::Compression;
use flate2::bufread::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};

use crate::jsonwrite::{push_i64, push_str, push_u64};
use crate::tree::{Node, NodeKind, Scan, ScanSettings, ScanStats, SizeMode, mtime_to_u32};

const FORMAT: &str = "cleanmcp-snapshot";
const VERSION: u32 = 1;

/// Rows are collected in a buffer this big before one `write` call: many small
/// writes (one per JSON token) make the gzip encoder an order of magnitude
/// slower than compressing whole chunks.
const WRITE_CHUNK: usize = 256 * 1024;

const READ_BUF: usize = 256 * 1024;

#[derive(Serialize, Deserialize)]
struct Header {
    format: String,
    version: u32,
    // `serde` refuses a `PathBuf` that is not valid UTF-8, which would make
    // `snapshot` fail after a successful scan of such a root on Linux. The
    // display path is lossy everywhere else, so store that.
    #[serde(serialize_with = "serialize_lossy_path")]
    root: PathBuf,
    scanned_at: i64,
    elapsed_ms: u64,
    #[serde(default)]
    settings: ScanSettings,
    #[serde(default)]
    stats: ScanStats,
}

fn serialize_lossy_path<S: serde::Serializer>(p: &Path, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&p.to_string_lossy())
}

/// The metadata in a snapshot's first line, without the tree.
#[derive(Debug, Clone)]
pub struct SnapshotHeader {
    pub root: PathBuf,
    pub scanned_at: i64,
    pub elapsed_ms: u64,
    pub settings: ScanSettings,
    pub stats: ScanStats,
}

fn kind_code(k: NodeKind) -> u8 {
    match k {
        NodeKind::File => 0,
        NodeKind::Dir => 1,
        NodeKind::Link => 2,
        NodeKind::Other => 3,
    }
}

fn kind_from(c: u8) -> NodeKind {
    match c {
        0 => NodeKind::File,
        1 => NodeKind::Dir,
        2 => NodeKind::Link,
        _ => NodeKind::Other,
    }
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn is_gz(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("gz"))
}

fn open_reader(path: &Path) -> io::Result<Box<dyn BufRead>> {
    let file = BufReader::with_capacity(READ_BUF, File::open(path)?);
    Ok(if is_gz(path) {
        Box::new(BufReader::with_capacity(READ_BUF, GzDecoder::new(file)))
    } else {
        Box::new(file)
    })
}

/// Write `scan` to `path` (gzip if it ends in `.gz`).
///
/// Writes to a sibling temp file and renames it into place, so a failed or
/// interrupted save cannot destroy the snapshot it was replacing.
pub fn save(scan: &Scan, path: &Path) -> io::Result<()> {
    let tmp = tmp_path(path);
    let result = write_to(scan, path, &tmp);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
        return result;
    }
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// `home.jsonl.gz` -> `home.jsonl.gz.tmp`.
fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

fn write_to(scan: &Scan, path: &Path, tmp: &Path) -> io::Result<()> {
    let file = File::create(tmp)?;
    if is_gz(path) {
        save_gz(scan, file)
    } else {
        let mut file = file;
        write_rows(scan, |chunk| file.write_all(&chunk))?;
        file.flush()?;
        // The rename can reach the disk before the data does; without this the
        // previous good snapshot could be replaced by an empty file on a crash.
        file.sync_all()
    }
}

/// Serializing rows and compressing them are both CPU-bound, so with gzip the
/// rows are handed to a second thread through a small channel: filling the next
/// chunk overlaps with compressing the previous one.
fn save_gz(scan: &Scan, file: File) -> io::Result<()> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(4);
    std::thread::scope(|scope| {
        let writer = scope.spawn(move || -> io::Result<()> {
            let mut w = GzEncoder::new(
                BufWriter::with_capacity(WRITE_CHUNK, file),
                Compression::fast(),
            );
            for chunk in rx {
                w.write_all(&chunk)?;
            }
            let mut out = w.finish()?;
            out.flush()?;
            // Same durability rule as the plain path: data before rename.
            out.get_ref().sync_all()
        });
        let produced = write_rows(scan, |chunk| {
            tx.send(chunk)
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "gzip writer stopped"))
        });
        drop(tx);
        let written = writer
            .join()
            .map_err(|_| io::Error::other("gzip writer thread panicked"))?;
        produced.and(written)
    })
}

fn write_rows(scan: &Scan, mut emit: impl FnMut(Vec<u8>) -> io::Result<()>) -> io::Result<()> {
    let mut chunk: Vec<u8> = Vec::with_capacity(WRITE_CHUNK + 1024);
    let header = Header {
        format: FORMAT.into(),
        version: VERSION,
        root: scan.root.clone(),
        scanned_at: scan.scanned_at,
        elapsed_ms: scan.elapsed_ms,
        settings: scan.settings.clone(),
        stats: scan.stats.clone(),
    };
    serde_json::to_writer(&mut chunk, &header)?;
    chunk.push(b'\n');

    // Iterative DFS so deep trees cannot overflow the stack.
    let mut stack: Vec<(u32, &Node)> = vec![(0, &scan.tree)];
    while let Some((depth, n)) = stack.pop() {
        chunk.push(b'[');
        push_u64(&mut chunk, depth as u64);
        chunk.push(b',');
        push_u64(&mut chunk, kind_code(n.kind()) as u64);
        chunk.push(b',');
        push_str(&mut chunk, &n.name);
        chunk.push(b',');
        push_u64(&mut chunk, n.size);
        chunk.push(b',');
        push_u64(&mut chunk, n.disk_size);
        chunk.push(b',');
        push_i64(&mut chunk, n.mtime_unix());
        chunk.push(b',');
        push_u64(&mut chunk, own_errors(n) as u64);
        chunk.extend_from_slice(b"]\n");
        if chunk.len() >= WRITE_CHUNK {
            emit(std::mem::replace(
                &mut chunk,
                Vec::with_capacity(WRITE_CHUNK + 1024),
            ))?;
        }
        for c in n.children().iter().rev() {
            stack.push((depth + 1, c));
        }
    }
    if !chunk.is_empty() {
        emit(chunk)?;
    }
    Ok(())
}

/// Errors recorded on this node itself (directories store the subtree total).
fn own_errors(n: &Node) -> u32 {
    let below: u32 = n
        .children()
        .iter()
        .map(|c| c.errors())
        .fold(0, u32::saturating_add);
    n.errors().saturating_sub(below)
}

fn read_header_line(r: &mut dyn BufRead, path: &Path) -> io::Result<Header> {
    let mut line = Vec::with_capacity(1024);
    read_until_newline(r, &mut line)?;
    let line = trim_eol(&line);
    if line.is_empty() {
        return Err(invalid("empty snapshot"));
    }
    let header: Header = serde_json::from_slice(line)?;
    if header.format != FORMAT || header.version != VERSION {
        return Err(invalid(format!(
            "{} is not a cleanmcp snapshot (version {VERSION})",
            path.display()
        )));
    }
    Ok(header)
}

fn read_until_newline(r: &mut dyn BufRead, line: &mut Vec<u8>) -> io::Result<usize> {
    line.clear();
    r.read_until(b'\n', line)
}

fn trim_eol(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 && (line[end - 1] == b'\n' || line[end - 1] == b'\r') {
        end -= 1;
    }
    &line[..end]
}

/// Metadata from a snapshot's header line, without reading the tree.
pub fn read_header(path: &Path) -> io::Result<SnapshotHeader> {
    let mut reader = open_reader(path)?;
    let h = read_header_line(&mut *reader, path)?;
    Ok(SnapshotHeader {
        root: h.root,
        scanned_at: h.scanned_at,
        elapsed_ms: h.elapsed_ms,
        settings: h.settings,
        stats: h.stats,
    })
}

/// Read a snapshot written by [`save`].
pub fn load(path: &Path) -> io::Result<Scan> {
    load_truncated(path, usize::MAX)
}

/// Read a snapshot but keep only nodes down to `max_depth` (the root is depth 0).
///
/// Sizes are the totals recorded in the snapshot (they are not recomputed from
/// the kept children), and `file_count`/`dir_count` cover only the kept nodes.
/// `diff` uses this: it never looks below its `--depth`, so pruning there is
/// invisible in the result and saves most of the second tree's memory.
pub fn load_truncated(path: &Path, max_depth: usize) -> io::Result<Scan> {
    let mut reader = open_reader(path)?;
    let header = read_header_line(&mut *reader, path)?;

    // Stack of open directories; children are attached when a directory closes.
    let mut stack: Vec<(Node, Vec<Node>)> = Vec::new();
    let mut root: Option<Node> = None;
    let mut line = Vec::with_capacity(1024);
    let mut prev_depth = 0usize;
    // A full load can check that every directory still equals its children: a
    // file cut between rows would otherwise load as a smaller, plausible tree.
    let verify = max_depth == usize::MAX;
    loop {
        let n = read_until_newline(&mut *reader, &mut line)?;
        if n == 0 {
            break;
        }
        let row = trim_eol(&line);
        if row.is_empty() {
            continue;
        }
        let (depth, kind, name, size, disk, mtime, errors) = parse_row(row)?;
        if root.is_none() && stack.is_empty() {
            if depth != 0 {
                return Err(invalid("snapshot must start with the root node"));
            }
        } else if depth > prev_depth + 1 {
            return Err(invalid("snapshot nodes out of order"));
        }
        prev_depth = depth;
        while stack.len() > depth {
            close(&mut stack, &mut root, verify)?;
        }
        if root.is_some() {
            return Err(invalid("snapshot has more than one root node"));
        }
        if depth > max_depth {
            continue;
        }
        let kind = kind_from(kind);
        let mut node = Node::new(kind, name, size, disk, mtime_to_u32(mtime));
        node.set_counts((kind == NodeKind::File) as u32, 0, errors);
        stack.push((node, Vec::new()));
    }
    while !stack.is_empty() {
        close(&mut stack, &mut root, verify)?;
    }
    Ok(Scan {
        root: header.root,
        tree: root.ok_or_else(|| invalid("snapshot has no nodes"))?,
        scanned_at: header.scanned_at,
        started_at: header.scanned_at,
        elapsed_ms: header.elapsed_ms,
        stats: header.stats,
        settings: header.settings,
    })
}

/// Finish a directory: attach its children and fill in the counters.
fn close(
    stack: &mut Vec<(Node, Vec<Node>)>,
    root: &mut Option<Node>,
    verify: bool,
) -> io::Result<()> {
    let (mut node, kids) = stack.pop().expect("non-empty");
    if !node.is_dir() && !kids.is_empty() {
        return Err(invalid("snapshot has children under a non-directory"));
    }
    if node.is_dir() {
        let mut files = 0u32;
        let mut dirs = 0u32;
        // The row stores this node's *own* errors; the tree wants the subtree total.
        let mut errors = node.errors();
        let (mut size, mut disk) = (0u64, 0u64);
        for c in &kids {
            files = files.saturating_add(c.file_count());
            dirs = dirs
                .saturating_add(c.dir_count())
                .saturating_add(c.is_dir() as u32);
            errors = errors.saturating_add(c.errors());
            size += c.size;
            disk += c.disk_size;
        }
        if verify && (node.size != size || node.disk_size != disk) {
            return Err(invalid(
                "snapshot is truncated or corrupt: a folder does not match its children",
            ));
        }
        node.set_counts(files, dirs, errors);
    }
    let mut kids = kids;
    // Rows are written largest-first, so this is almost always a no-op.
    if !kids.windows(2).all(|w| w[0].disk_size >= w[1].disk_size) {
        kids.sort_unstable_by_key(|c| std::cmp::Reverse(c.disk_size));
    }
    node.set_children(kids);
    match stack.last_mut() {
        Some((_, siblings)) => siblings.push(node),
        None => *root = Some(node),
    }
    Ok(())
}

/// One row: `[depth, kind, name, size, disk_size, mtime, errors]`.
fn parse_row(line: &[u8]) -> io::Result<(usize, u8, CompactString, u64, u64, i64, u32)> {
    let mut p = Parser::new(line);
    p.expect(b'[')?;
    let depth = p.uint()?;
    p.expect(b',')?;
    let kind = p.uint()?;
    p.expect(b',')?;
    let name = p.string()?;
    p.expect(b',')?;
    let size = p.uint()?;
    p.expect(b',')?;
    let disk = p.uint()?;
    p.expect(b',')?;
    let mtime = p.int()?;
    p.expect(b',')?;
    let errors = p.uint()?;
    p.expect(b']')?;
    p.skip_ws();
    if !p.at_end() {
        return Err(invalid("trailing data after snapshot row"));
    }
    Ok((
        usize::try_from(depth).map_err(|_| invalid("depth too large"))?,
        u8::try_from(kind).map_err(|_| invalid("bad node kind"))?,
        name,
        size,
        disk,
        mtime,
        u32::try_from(errors).map_err(|_| invalid("error count too large"))?,
    ))
}

/// A minimal parser for the row grammar above, with `serde_json`-compatible
/// string unescaping.
struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, i: 0 }
    }

    #[inline]
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    #[inline]
    fn at_end(&self) -> bool {
        self.i >= self.b.len()
    }

    #[inline]
    fn expect(&mut self, c: u8) -> io::Result<()> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            Err(invalid(format!(
                "expected `{}` at byte {} of a snapshot row",
                c as char, self.i
            )))
        }
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r')) {
            self.i += 1;
        }
    }

    fn uint(&mut self) -> io::Result<u64> {
        let start = self.i;
        let mut v: u64 = 0;
        while let Some(c) = self.peek() {
            if !c.is_ascii_digit() {
                break;
            }
            v = v
                .checked_mul(10)
                .and_then(|v| v.checked_add((c - b'0') as u64))
                .ok_or_else(|| invalid("number too large in snapshot row"))?;
            self.i += 1;
        }
        if self.i == start {
            return Err(invalid("expected a number in snapshot row"));
        }
        Ok(v)
    }

    fn int(&mut self) -> io::Result<i64> {
        let neg = self.peek() == Some(b'-');
        if neg {
            self.i += 1;
        }
        let v = self.uint()?;
        if neg {
            if v > i64::MAX as u64 + 1 {
                return Err(invalid("number too small in snapshot row"));
            }
            Ok((v as i64).wrapping_neg())
        } else {
            i64::try_from(v).map_err(|_| invalid("number too large in snapshot row"))
        }
    }

    fn hex4(&mut self) -> io::Result<u16> {
        let mut v: u16 = 0;
        for _ in 0..4 {
            let c = self
                .peek()
                .ok_or_else(|| invalid("truncated \\u escape in snapshot row"))?;
            self.i += 1;
            let d = (c as char)
                .to_digit(16)
                .ok_or_else(|| invalid("bad \\u escape in snapshot row"))?;
            v = (v << 4) | d as u16;
        }
        Ok(v)
    }

    fn string(&mut self) -> io::Result<CompactString> {
        self.expect(b'"')?;
        let start = self.i;
        while let Some(c) = self.peek() {
            match c {
                b'"' => {
                    let s = std::str::from_utf8(&self.b[start..self.i])
                        .map_err(|_| invalid("invalid UTF-8 in snapshot name"))?;
                    self.i += 1;
                    return Ok(CompactString::from(s));
                }
                b'\\' => break,
                c if c < 0x20 => {
                    return Err(invalid("raw control character in snapshot name"));
                }
                _ => self.i += 1,
            }
        }
        if self.at_end() {
            return Err(invalid("unterminated string in snapshot row"));
        }

        // Slow path: the name contains escapes.
        let mut out = String::with_capacity(self.i - start + 16);
        out.push_str(
            std::str::from_utf8(&self.b[start..self.i])
                .map_err(|_| invalid("invalid UTF-8 in snapshot name"))?,
        );
        loop {
            // `self.i` is on a backslash.
            self.i += 1;
            let c = self
                .peek()
                .ok_or_else(|| invalid("unterminated string in snapshot row"))?;
            self.i += 1;
            match c {
                b'"' => out.push('"'),
                b'\\' => out.push('\\'),
                b'/' => out.push('/'),
                b'b' => out.push('\u{8}'),
                b'f' => out.push('\u{c}'),
                b'n' => out.push('\n'),
                b'r' => out.push('\r'),
                b't' => out.push('\t'),
                b'u' => {
                    let hi = self.hex4()?;
                    let ch = if (0xD800..0xDC00).contains(&hi) {
                        if self.peek() != Some(b'\\') {
                            return Err(invalid("lone surrogate escape in snapshot name"));
                        }
                        self.i += 1;
                        if self.peek() != Some(b'u') {
                            return Err(invalid("lone surrogate escape in snapshot name"));
                        }
                        self.i += 1;
                        let lo = self.hex4()?;
                        if !(0xDC00..0xE000).contains(&lo) {
                            return Err(invalid("bad surrogate pair in snapshot name"));
                        }
                        let cp = 0x1_0000 + (((hi - 0xD800) as u32) << 10) + (lo - 0xDC00) as u32;
                        char::from_u32(cp)
                            .ok_or_else(|| invalid("bad code point in snapshot name"))?
                    } else {
                        char::from_u32(hi as u32)
                            .ok_or_else(|| invalid("bad code point in snapshot name"))?
                    };
                    out.push(ch);
                }
                _ => return Err(invalid("invalid escape in snapshot name")),
            }

            let start = self.i;
            while let Some(b) = self.peek() {
                match b {
                    b'"' => {
                        out.push_str(
                            std::str::from_utf8(&self.b[start..self.i])
                                .map_err(|_| invalid("invalid UTF-8 in snapshot name"))?,
                        );
                        self.i += 1;
                        return Ok(CompactString::from(out));
                    }
                    b'\\' => {
                        out.push_str(
                            std::str::from_utf8(&self.b[start..self.i])
                                .map_err(|_| invalid("invalid UTF-8 in snapshot name"))?,
                        );
                        break;
                    }
                    b if b < 0x20 => {
                        return Err(invalid("raw control character in snapshot name"));
                    }
                    _ => self.i += 1,
                }
            }
            if self.at_end() {
                return Err(invalid("unterminated string in snapshot row"));
            }
        }
    }
}

/// How one path changed between two scans.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DiffNode {
    pub path: PathBuf,
    pub old_size: u64,
    pub new_size: u64,
    /// `new_size - old_size` (negative = shrank).
    pub delta: i64,
    /// "added", "removed" or "changed".
    pub status: &'static str,
    pub is_dir: bool,
    /// Changed children, largest absolute change first (limited by depth/top).
    pub children: Vec<DiffNode>,
    /// Number of changed children not listed because of `top`.
    pub more: usize,
}

/// Compare two trees. Only paths whose size changed are included.
pub fn diff(
    old: &Node,
    new: &Node,
    base: &Path,
    depth: usize,
    top: usize,
    mode: SizeMode,
) -> DiffNode {
    diff_rec(Some(old), Some(new), base, depth, top, mode)
}

fn diff_rec(
    old: Option<&Node>,
    new: Option<&Node>,
    path: &Path,
    depth: usize,
    top: usize,
    mode: SizeMode,
) -> DiffNode {
    let old_size = old.map_or(0, |n| n.size_by(mode));
    let new_size = new.map_or(0, |n| n.size_by(mode));
    let status = match (old, new) {
        (None, _) => "added",
        (_, None) => "removed",
        _ => "changed",
    };
    let is_dir = new.or(old).is_some_and(|n| n.is_dir());
    let mut children = Vec::new();
    let mut more = 0;
    if depth > 0 && is_dir {
        let mut changed: Vec<(i64, Option<&Node>, Option<&Node>)> = Vec::new();
        let olds: std::collections::HashMap<&str, &Node> = old
            .map(|n| n.children().iter().map(|c| (c.name.as_str(), c)).collect())
            .unwrap_or_default();
        let mut seen = std::collections::HashSet::new();
        for c in new.map(|n| n.children()).unwrap_or_default() {
            seen.insert(c.name.as_str());
            let o = olds.get(c.name.as_str()).copied();
            let d = c.size_by(mode) as i64 - o.map_or(0, |o| o.size_by(mode) as i64);
            if d != 0 || o.is_none() {
                changed.push((d, o, Some(c)));
            }
        }
        for (name, o) in &olds {
            if !seen.contains(name) {
                changed.push((-(o.size_by(mode) as i64), Some(*o), None));
            }
        }
        changed.sort_by_key(|(d, ..)| std::cmp::Reverse(d.unsigned_abs()));
        more = changed.len().saturating_sub(top);
        children = changed
            .into_iter()
            .take(top)
            .map(|(_, o, n)| {
                let name = n.or(o).map(|x| x.name.as_str()).unwrap_or_default();
                diff_rec(o, n, &path.join(name), depth - 1, top, mode)
            })
            .collect();
    }
    DiffNode {
        path: path.to_path_buf(),
        old_size,
        new_size,
        delta: new_size as i64 - old_size as i64,
        status,
        is_dir,
        children,
        more,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{ScanOptions, scan};
    use std::fs;

    /// A snapshot written by cleanmcp 0.1.0 (which serialized every row with
    /// `serde_json`). Names here cover escapes, a non-UTF-8 lossy name is not
    /// representable, so this is the "old file still loads" fixture.
    const V010_SNAPSHOT: &str = concat!(
        r#"{"format":"cleanmcp-snapshot","version":1,"root":"/tmp/old","scanned_at":1700000000,"elapsed_ms":12,"settings":{"one_filesystem":false,"include_virtual":false,"dedupe_hardlinks":false,"exclude":["*.tmp"]},"stats":{"hardlinks_deduped":0,"skipped_mounts":0,"excluded":1,"cancelled":false}}"#,
        "\n",
        r#"[0,1,"old",7000,4096,1700000000,0]"#,
        "\n",
        r#"[1,1,"a",5000,4096,1700000000,0]"#,
        "\n",
        r#"[2,0,"quote\"name\t\u0001",5000,4096,1700000000,0]"#,
        "\n",
        r#"[1,0,"gone.txt",2000,0,-1000,1]"#,
        "\n",
        r#"[1,2,"link",0,0,1700000000,0]"#,
        "\n",
    );

    fn compare(a: &Node, b: &Node) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.kind(), b.kind());
        assert_eq!(a.size, b.size);
        assert_eq!(a.disk_size, b.disk_size);
        assert_eq!(a.mtime, b.mtime);
        assert_eq!(a.file_count(), b.file_count(), "file_count of {}", a.name);
        assert_eq!(a.dir_count(), b.dir_count(), "dir_count of {}", a.name);
        assert_eq!(a.errors(), b.errors());
        assert_eq!(a.children().len(), b.children().len());
        for (x, y) in a.children().iter().zip(b.children().iter()) {
            compare(x, y);
        }
    }

    #[test]
    fn save_replaces_atomically_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("data");
        fs::create_dir_all(&p).unwrap();
        fs::write(p.join("f"), vec![1u8; 100]).unwrap();
        let s = scan(&p, &ScanOptions::default()).unwrap();
        let out = dir.path().join("s.jsonl");
        save(&s, &out).unwrap();
        let first = fs::read_to_string(&out).unwrap();
        save(&s, &out).unwrap();
        assert_eq!(first, fs::read_to_string(&out).unwrap());
        assert!(
            !dir.path().join("s.jsonl.tmp").exists(),
            "temp file left behind"
        );
    }

    #[test]
    fn roundtrip_and_diff() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("data");
        fs::create_dir_all(p.join("a/deep/er")).unwrap();
        fs::write(p.join("a/deep/er/f"), vec![1u8; 1000]).unwrap();
        fs::write(p.join("gone"), vec![1u8; 300]).unwrap();
        let before = scan(&p, &ScanOptions::default()).unwrap();

        for name in ["snap.jsonl", "snap.jsonl.gz"] {
            let file = dir.path().join(name);
            save(&before, &file).unwrap();
            let loaded = load(&file).unwrap();
            assert_eq!(loaded.root, before.root);
            compare(&loaded.tree, &before.tree);
            assert_eq!(loaded.settings, before.settings);
            assert_eq!(loaded.find(Path::new("a/deep/er/f")).unwrap().size, 1000);
        }

        fs::remove_file(p.join("gone")).unwrap();
        fs::write(p.join("a/new"), vec![1u8; 5000]).unwrap();
        let after = scan(&p, &ScanOptions::default()).unwrap();
        let d = diff(
            &before.tree,
            &after.tree,
            &after.root,
            3,
            10,
            SizeMode::Apparent,
        );
        assert_eq!(d.delta, 5000 - 300);
        assert_eq!(d.children[0].status, "changed"); // a/ grew by 5000
        assert_eq!(d.children[0].children[0].status, "added");
        assert_eq!(d.children[1].status, "removed");
    }

    #[test]
    fn odd_names_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("data");
        fs::create_dir_all(&p).unwrap();
        // Quotes and backslashes are illegal in Windows file names, so the
        // escape-heavy cases are unix-only; `jsonwrite` and the old-snapshot
        // fixture cover escaping on both platforms.
        let mut names = vec!["hÃƒÂ©llo Ã°Å¸Å¡â‚¬", "comma,name", "sp ace"];
        if cfg!(unix) {
            names.push("quote\"inside");
            names.push("back\\slash");
        }
        for n in &names {
            fs::write(p.join(n), vec![1u8; 16]).unwrap();
        }
        let s = scan(&p, &ScanOptions::default()).unwrap();
        for file in ["x.jsonl", "x.jsonl.gz"] {
            let f = dir.path().join(file);
            save(&s, &f).unwrap();
            let back = load(&f).unwrap();
            compare(&back.tree, &s.tree);
            for n in &names {
                assert!(
                    back.find(Path::new(n)).is_some(),
                    "{n:?} missing from {file}"
                );
            }
        }
    }

    #[test]
    fn errors_roll_up_from_a_loaded_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("data");
        fs::create_dir_all(p.join("sub")).unwrap();
        fs::write(p.join("sub/leaf"), b"x").unwrap();
        let mut s = scan(&p, &ScanOptions::default()).unwrap();
        // The file could not be read: the error belongs to the subtree totals.
        let sub = s
            .tree
            .children_mut()
            .iter_mut()
            .find(|c| &*c.name == "sub")
            .unwrap();
        sub.children_mut()[0].add_error();
        sub.finish_dir();
        s.tree.finish_dir();
        assert_eq!(s.tree.errors(), 1);

        let f = dir.path().join("e.jsonl");
        save(&s, &f).unwrap();
        let back = load(&f).unwrap();
        assert_eq!(back.tree.errors(), 1, "subtree error count lost on load");
        assert_eq!(back.find(Path::new("sub")).unwrap().errors(), 1);
    }

    #[test]
    fn rejects_a_snapshot_cut_between_rows() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("data");
        fs::create_dir_all(p.join("sub")).unwrap();
        fs::write(p.join("sub/leaf"), b"xxxxxxxxxx").unwrap();
        let s = scan(&p, &ScanOptions::default()).unwrap();
        let full = dir.path().join("full.jsonl");
        save(&s, &full).unwrap();

        let text = fs::read_to_string(&full).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        // Header plus the root row: the root's recorded size no longer matches.
        let cut = dir.path().join("cut.jsonl");
        fs::write(&cut, format!("{}\n{}\n", lines[0], lines[1])).unwrap();
        assert!(load(&cut).is_err(), "truncated snapshot loaded as valid");
    }

    #[test]
    fn rejects_two_roots_and_children_under_files() {
        let dir = tempfile::tempdir().unwrap();
        let header = r#"{"format":"cleanmcp-snapshot","version":1,"root":"/x","scanned_at":0,"elapsed_ms":0}"#;
        let two = dir.path().join("two.jsonl");
        fs::write(
            &two,
            format!("{header}\n[0,1,\"a\",0,0,0,0]\n[0,1,\"b\",0,0,0,0]\n"),
        )
        .unwrap();
        assert!(load(&two).is_err(), "second root was silently dropped");

        let kids = dir.path().join("kids.jsonl");
        fs::write(
            &kids,
            format!("{header}\n[0,0,\"f\",1,1,0,0]\n[1,0,\"c\",1,1,0,0]\n"),
        )
        .unwrap();
        assert!(load(&kids).is_err(), "file node accepted children");
    }

    #[test]
    fn loads_v010_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("old.jsonl");
        fs::write(&f, V010_SNAPSHOT).unwrap();
        let s = load(&f).unwrap();
        assert_eq!(s.root, Path::new("/tmp/old"));
        assert_eq!(s.scanned_at, 1_700_000_000);
        assert_eq!(s.settings.exclude, ["*.tmp"]);
        assert_eq!(s.stats.excluded, 1);
        assert_eq!(s.tree.size, 7000);
        assert_eq!(s.tree.file_count(), 2);
        assert_eq!(s.tree.dir_count(), 1);
        assert_eq!(s.tree.errors(), 1);
        assert_eq!(s.tree.children().len(), 3);
        let a = s.find(Path::new("a")).unwrap();
        assert_eq!(a.size, 5000);
        assert_eq!(a.file_count(), 1);
        assert_eq!(
            s.find(Path::new("a/quote\"name\t\u{1}")).unwrap().size,
            5000
        );
        let gone = s.find(Path::new("gone.txt")).unwrap();
        // `Node::mtime` stores u32 seconds, so pre-1970 times clamp to the epoch.
        assert_eq!(gone.mtime_unix(), 0);
        assert_eq!(gone.kind(), NodeKind::File);
        assert_eq!(gone.errors(), 1);
        assert_eq!(s.find(Path::new("link")).unwrap().kind(), NodeKind::Link);
        // Gzipped old snapshots also load.
        let gz = dir.path().join("old.jsonl.gz");
        {
            use std::io::Write as _;
            let mut w = GzEncoder::new(fs::File::create(&gz).unwrap(), Compression::fast());
            w.write_all(V010_SNAPSHOT.as_bytes()).unwrap();
            w.finish().unwrap();
        }
        compare(&load(&gz).unwrap().tree, &s.tree);
    }

    #[test]
    fn truncated_load_keeps_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("data");
        fs::create_dir_all(p.join("a/b/c")).unwrap();
        fs::write(p.join("a/b/c/f"), vec![1u8; 4096]).unwrap();
        fs::write(p.join("top"), vec![1u8; 100]).unwrap();
        let s = scan(&p, &ScanOptions::default()).unwrap();
        let f = dir.path().join("s.jsonl.gz");
        save(&s, &f).unwrap();

        let shallow = load_truncated(&f, 1).unwrap();
        assert_eq!(shallow.tree.size, s.tree.size);
        assert_eq!(shallow.tree.disk_size, s.tree.disk_size);
        let a = shallow.find(Path::new("a")).unwrap();
        assert_eq!(a.disk_size, s.find(Path::new("a")).unwrap().disk_size);
        assert!(
            a.children().is_empty(),
            "children below depth 1 are dropped"
        );

        // Diffing the truncated snapshot gives the same result as diffing the
        // full one, as long as `depth` does not go below the truncation.
        let full = load(&f).unwrap();
        let d_full = diff(&full.tree, &s.tree, &s.root, 1, 10, SizeMode::Disk);
        let d_shallow = diff(&shallow.tree, &s.tree, &s.root, 1, 10, SizeMode::Disk);
        assert_eq!(format!("{d_full:?}"), format!("{d_shallow:?}"));
    }

    #[test]
    fn rejects_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("bad.jsonl");
        let header = r#"{"format":"cleanmcp-snapshot","version":1,"root":"/x","scanned_at":0,"elapsed_ms":0}"#;
        for row in [
            "[1,1,\"too deep\"]",
            "[0,1,\"root\"]\n[2,0,\"skip\"]",
            "[0,9,\"bad kind\"]",
            "[0,1,\"unterminated]",
            "[0,1,\"bad\\qescape\"]",
        ] {
            fs::write(&f, format!("{header}\n{row}\n")).unwrap();
            assert!(load(&f).is_err(), "should reject {row:?}");
        }
        // A valid file with a UTF-8 BOM or wrong format also fails.
        fs::write(&f, "{\"format\":\"other\",\"version\":1,\"root\":\"/x\",\"scanned_at\":0,\"elapsed_ms\":0}\n").unwrap();
        assert!(load(&f).is_err());
    }

    #[test]
    fn writes_same_rows_as_serde_json() {
        // The row grammar must stay byte-identical to 0.1.0's serde_json output,
        // so that old binaries can read new snapshots. This test checks the
        // escaping/formatting helpers are used consistently for a tricky name.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("data");
        fs::create_dir_all(&p).unwrap();
        // `"` is not a legal Windows file name; the unicode name still checks
        // the common path on both platforms.
        let name = if cfg!(unix) {
            "quo\"te ÃƒÂ©"
        } else {
            "quo ÃƒÂ©"
        };
        fs::write(p.join(name), vec![1u8; 8]).unwrap();
        let s = scan(&p, &ScanOptions::default()).unwrap();
        let node = s.find(Path::new(name)).unwrap();
        let f = dir.path().join("s.jsonl");
        save(&s, &f).unwrap();
        let text = fs::read_to_string(&f).unwrap();
        let escaped = serde_json::to_string(name).unwrap();
        let row = text
            .lines()
            .find(|l| l.contains(&escaped))
            .expect("escaped name in row");
        let expected = serde_json::to_string(&(
            1u32,
            kind_code(NodeKind::File),
            name,
            node.size,
            node.disk_size,
            node.mtime_unix(),
            0u32,
        ))
        .unwrap();
        assert_eq!(row, expected);
    }
}
