//! MCP server: disk usage tools for AI agents, over stdio or HTTP.
//!
//! Scans are cached in memory per server process, so an agent can scan a drive
//! once and then drill down without rescanning. All tools are read-only unless
//! the server is started with `--allow-delete`, which adds `move_to_trash`.

use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::Result;
use cleanmcp_core::config::Config;
use cleanmcp_core::drives::{self, Drive};
use cleanmcp_core::dupes::{self, DuplicateGroup};
use cleanmcp_core::query::{self, AgeStat, Entry, ExtensionStat, FileFilter, format_time};
use cleanmcp_core::rules::{self, Candidate, RuleSet};
use cleanmcp_core::snapshot::{self, DiffNode};
use cleanmcp_core::tree::{is_within, path_key, same_path};
use cleanmcp_core::{Node, Scan, ScanOptions, SizeMode};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{
    Implementation, ListResourcesResult, PaginatedRequestParams, ProgressNotificationParam,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, RequestMetaObject,
    Resource, ResourceContents, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{
    ErrorData, Json, Peer, RoleServer, ServerHandler, ServiceExt, tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::output::{TreeJson, human};
use crate::remove::{self, Removal};

/// Keep at most this many scans in memory (a whole-drive scan can use ~1 GB).
const MAX_CACHED_SCANS: usize = 4;
const SCANS_URI: &str = "cleanmcp://scans";
/// Largest accepted pagination offset.
const MAX_OFFSET: usize = 100_000;
/// Longest path string accepted from a client. Above any real path: Windows
/// allows 32767 UTF-16 units (up to ~96 KiB as UTF-8), Linux `PATH_MAX` is 4 KiB.
const MAX_PATH_LEN: usize = 128 * 1024;
/// Most paths one `move_to_trash` call may name. Each costs a parent lookup and
/// an audit line, so an unbounded list is a (possibly very slow) denial of service.
const MAX_TRASH_PATHS: usize = 256;
/// Longest `reason` copied into the audit log, once per path.
const MAX_REASON_LEN: usize = 4 * 1024;
/// Most paths a session remembers as "shown". Past this, new paths are not
/// remembered; deleting one is then refused, which is the safe way to fail.
const MAX_SEEN_PATHS: usize = 100_000;

/// Cancels a running scan when the request that started it is dropped (client
/// disconnect, shutdown, or an aborted handler). `spawn_blocking` tasks are not
/// cancelled by dropping their handle, so the walker needs this signal.
struct CancelOnDrop(Arc<cleanmcp_core::scan::ScanControl>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// How the server was started.
#[derive(Debug, Clone, Default)]
pub struct McpOptions {
    /// Only allow paths inside these folders. Empty = anywhere.
    pub roots: Vec<PathBuf>,
    /// Register the `move_to_trash` tool.
    pub allow_delete: bool,
    /// Serve over HTTP on this address instead of stdio.
    pub http: Option<SocketAddr>,
}

pub async fn serve(cfg: Config, opts: McpOptions) -> Result<()> {
    if let Some(addr) = opts.http
        && opts.allow_delete
        && !addr.ip().is_loopback()
    {
        anyhow::bail!(
            "refusing --allow-delete on {addr}: the HTTP endpoint has no authentication, so anyone on the network could delete files. Bind to 127.0.0.1 instead."
        );
    }
    let server = CleanMcp::new(cfg, &opts)?;
    match opts.http {
        None => {
            let service = server.serve(rmcp::transport::stdio()).await?;
            service.waiting().await?;
        }
        Some(addr) => serve_http(server, addr).await?,
    }
    Ok(())
}

async fn serve_http(server: CleanMcp, addr: SocketAddr) -> Result<()> {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    };
    let mut config = StreamableHttpServerConfig::default();
    if addr.ip().is_loopback() {
        // Host-header check against DNS rebinding: localhost names plus the bound address.
        config = config.with_allowed_hosts([
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
            addr.ip().to_string(),
            addr.to_string(),
        ]);
    } else {
        tracing::warn!(
            "listening on {addr}: the MCP endpoint has no authentication; anyone who can reach it can read your file listing"
        );
        // Remote clients use whatever host name reaches this machine, so a Host
        // allow-list cannot be known here; the check only matters for localhost.
        config = config.disable_allowed_hosts();
    }
    let shutdown = config.cancellation_token.clone();
    // Each HTTP session gets its own "seen paths" list; the scan cache is shared.
    let service: StreamableHttpService<CleanMcp, LocalSessionManager> =
        StreamableHttpService::new(move || Ok(server.new_session()), Default::default(), config);
    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("cleanmcp MCP server listening on http://{addr}/mcp");
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            // Close open SSE streams so the server can actually exit.
            shutdown.cancel();
        })
        .await?;
    Ok(())
}

type SharedScan = Arc<RwLock<Scan>>;

#[derive(Clone)]
pub struct CleanMcp {
    cache: Arc<Mutex<Vec<SharedScan>>>,
    rules: Arc<RuleSet>,
    exclude: Arc<Vec<String>>,
    roots: Arc<Vec<PathBuf>>,
    /// Paths this session has shown the agent; only these may be trashed.
    /// Keyed by platform path identity (see [`path_key`]) so Windows case
    /// variants match and membership is O(1).
    seen: Arc<Mutex<HashMap<String, PathBuf>>>,
    tool_router: ToolRouter<Self>,
}

// ---- tool parameters -------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct ScanParams {
    /// Folder or drive to scan, e.g. "C:\\" or "/home/me".
    pub path: String,
    /// Rescan even if a cached scan covers this path.
    #[serde(default)]
    pub refresh: bool,
    /// Linux: do not cross into other mounted filesystems.
    #[serde(default)]
    pub one_filesystem: bool,
    /// Report logical file sizes instead of space used on disk.
    pub apparent: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct PathParams {
    pub path: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct DrivesParams {
    /// Also list pseudo filesystems (tmpfs, squashfs, ...).
    pub include_virtual: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ChildrenParams {
    pub path: String,
    /// Levels to expand (default 1, max 4).
    pub depth: Option<usize>,
    /// Children per folder (default 20, max 200). Lowered automatically when
    /// `depth > 1` so one response stays under 20k rows.
    pub top: Option<usize>,
    /// Skip this many children at the first level (pagination).
    pub offset: Option<usize>,
    /// Report logical file sizes instead of space used on disk.
    pub apparent: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct FilesParams {
    pub path: String,
    /// How many files to return (default 25, max 500).
    pub top: Option<usize>,
    /// Skip this many results (pagination).
    pub offset: Option<usize>,
    /// Only files at least this many MiB.
    pub min_size_mib: Option<u64>,
    /// Only files not modified for at least this many days.
    pub older_than_days: Option<u64>,
    /// Only files modified less than this many days ago.
    pub newer_than_days: Option<u64>,
    /// Only these extensions, e.g. ["iso", "zip"].
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Report logical file sizes instead of space used on disk.
    pub apparent: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct TopParams {
    pub path: String,
    /// How many rows (default 25).
    pub top: Option<usize>,
    /// Report logical file sizes instead of space used on disk.
    pub apparent: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SuggestParams {
    pub path: String,
    /// Ignore matches smaller than this many MiB (default 10).
    pub min_size_mib: Option<u64>,
    /// How many to return (default 30, max 500).
    pub top: Option<usize>,
    /// Skip this many results (pagination).
    pub offset: Option<usize>,
    /// Report logical file sizes instead of space used on disk.
    pub apparent: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct DupesParams {
    pub path: String,
    /// Ignore files smaller than this many MiB (default 1).
    pub min_size_mib: Option<u64>,
    /// Groups to return (default 20, max 200).
    pub top: Option<usize>,
    /// Skip this many groups (pagination). Each page re-hashes the files.
    pub offset: Option<usize>,
    /// Price wasted space by logical size instead of space used on disk.
    pub apparent: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SnapshotParams {
    /// Folder or drive to scan.
    pub path: String,
    /// File to write the snapshot to (`.gz` = compressed). Must be inside the
    /// folders this server may access.
    pub output: String,
    /// Rescan even if a cached scan covers this path.
    #[serde(default)]
    pub refresh: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct DiffParams {
    /// Snapshot file written by `save_snapshot` (or `cleanmcp snapshot`).
    pub old: String,
    /// Newer snapshot file, or a folder to scan now. Default: rescan the old
    /// snapshot's root.
    pub new: Option<String>,
    /// Levels to expand (default 3, max 6).
    pub depth: Option<usize>,
    /// Changed children per folder (default 10).
    pub top: Option<usize>,
    /// Report logical file sizes instead of space used on disk.
    pub apparent: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct TrashParams {
    /// Files or folders to move to the Recycle Bin / Trash. Each must have been
    /// returned by another tool in this session.
    pub paths: Vec<String>,
    /// Why these can go (recorded in the audit log).
    pub reason: Option<String>,
}

// ---- tool results (MCP structured output must be an object) ----------------

#[derive(Serialize, JsonSchema)]
pub struct ScanSummary {
    pub root: PathBuf,
    /// Allocated size on disk in bytes (what deleting frees).
    pub disk_size: u64,
    /// Logical (apparent) size in bytes.
    pub size: u64,
    pub disk_size_human: String,
    pub file_count: u64,
    pub dir_count: u64,
    /// Entries that could not be read (usually permission denied).
    pub errors: u64,
    /// Entries skipped by the configured `exclude` globs or the virtual-fs filter.
    pub excluded_entries: u64,
    /// Directories skipped because they are on another filesystem.
    pub skipped_mounts: u64,
    /// Extra hard links to an already-counted file (counted as 0 bytes).
    pub hardlinks_deduped: u64,
    /// The scan was cancelled; totals are partial.
    pub cancelled: bool,
    /// Whether this scan stayed on one filesystem (echoes the request).
    pub one_filesystem: bool,
    pub elapsed_ms: u64,
    pub scanned_at_utc: String,
    /// Largest direct children, by size on disk.
    pub top_children: Vec<Entry>,
    /// Direct children not listed in `top_children`.
    pub other_children_count: usize,
    /// Disk size of the direct children not listed.
    pub other_children_disk_size: u64,
}

#[derive(Serialize, JsonSchema)]
pub struct DrivesResult {
    pub drives: Vec<Drive>,
}

#[derive(Serialize, JsonSchema)]
pub struct FilesResult {
    pub files: Vec<Entry>,
    /// Pass as `offset` to get the next page; absent when there are no more.
    pub next_offset: Option<usize>,
}

#[derive(Serialize, JsonSchema)]
pub struct ExtResult {
    pub extensions: Vec<ExtensionStat>,
}

#[derive(Serialize, JsonSchema)]
pub struct AgeResult {
    pub buckets: Vec<AgeStat>,
}

#[derive(Serialize, JsonSchema)]
pub struct SuggestResult {
    /// Sum of all matches' on-disk sizes (not just this page).
    pub total_disk_size: u64,
    pub total_disk_size_human: String,
    pub total_count: usize,
    pub candidates: Vec<Candidate>,
    pub next_offset: Option<usize>,
    pub note: &'static str,
}

#[derive(Serialize, JsonSchema)]
pub struct DupesResult {
    /// Space freed by keeping one copy of every group (all groups).
    pub total_wasted: u64,
    pub total_wasted_human: String,
    pub total_groups: usize,
    pub groups: Vec<DuplicateGroup>,
    /// Offset to pass next to continue listing; null when this is the last page.
    pub next_offset: Option<usize>,
}

#[derive(Serialize, JsonSchema)]
pub struct SnapshotResult {
    pub output: PathBuf,
    pub root: PathBuf,
    pub entries: u64,
    pub disk_size: u64,
    pub disk_size_human: String,
    pub scanned_at_utc: String,
}

#[derive(Serialize, JsonSchema)]
pub struct DiffResult {
    pub old_root: PathBuf,
    pub new_root: PathBuf,
    pub old_scanned_at_utc: String,
    pub new_scanned_at_utc: String,
    /// What grew or shrank, largest absolute change first.
    pub changes: DiffNode,
}

#[derive(Serialize, JsonSchema)]
pub struct TrashResult {
    pub results: Vec<Removal>,
    pub freed_disk_size: u64,
    pub freed_human: String,
}

fn page<T>(mut items: Vec<T>, offset: usize, top: usize) -> (Vec<T>, Option<usize>) {
    let total = items.len();
    let items: Vec<T> = items.drain(offset.min(total)..).take(top).collect();
    let next = (offset + items.len() < total).then_some(offset + items.len());
    (items, next)
}

/// Most nodes a single `list_children` response may contain.
const MAX_TREE_ROWS: usize = 20_000;

/// Lower `top` so `depth` levels of fan-out stay within [`MAX_TREE_ROWS`].
fn bounded_top(depth: usize, top: usize) -> usize {
    (1..=top)
        .rev()
        .find(|t| t.saturating_pow(depth as u32) <= MAX_TREE_ROWS)
        .unwrap_or(1)
}

fn mib(v: Option<u64>, default: u64) -> u64 {
    v.unwrap_or(default).saturating_mul(1024 * 1024)
}

fn mode(apparent: Option<bool>) -> SizeMode {
    match apparent {
        Some(true) => SizeMode::Apparent,
        _ => SizeMode::Disk,
    }
}

/// Reject requests that would make the server do an unbounded amount of work:
/// every path costs a parent lookup, and `reason` is copied into the audit log
/// once per path.
fn check_trash_request(paths: &[String], reason: Option<&str>) -> Result<(), String> {
    if paths.len() > MAX_TRASH_PATHS {
        return Err(format!(
            "too many paths ({}); move at most {MAX_TRASH_PATHS} per call",
            paths.len()
        ));
    }
    if let Some(p) = paths.iter().find(|p| p.len() > MAX_PATH_LEN) {
        return Err(format!(
            "path is too long ({} bytes, max {MAX_PATH_LEN})",
            p.len()
        ));
    }
    if let Some(r) = reason
        && r.len() > MAX_REASON_LEN
    {
        return Err(format!(
            "reason is too long ({} bytes, max {MAX_REASON_LEN})",
            r.len()
        ));
    }
    Ok(())
}

/// Split accepted targets into the paths to delete and the paths already
/// covered by one of them. Every kept parent is checked, not just the previous
/// path: an unrelated sibling (e.g. `/a-b`) can sort between a folder and its
/// child (`/a/b`), which would otherwise make the child a second delete target.
fn prune_covered(mut accepted: Vec<PathBuf>) -> (Vec<PathBuf>, Vec<(PathBuf, PathBuf)>) {
    accepted.sort();
    accepted.dedup();
    let mut top_level: Vec<PathBuf> = Vec::new();
    let mut covered: Vec<(PathBuf, PathBuf)> = Vec::new();
    for target in accepted {
        match top_level.iter().find(|parent| is_within(&target, parent)) {
            Some(parent) => covered.push((target, parent.to_path_buf())),
            None => top_level.push(target),
        }
    }
    (top_level, covered)
}

/// The result for a path that was not deleted separately because its parent
/// folder was in the same request. It is only a success if the parent moved.
fn covered_result(child: PathBuf, parent: &Path, parent_ok: bool) -> Removal {
    Removal {
        path: child,
        ok: parent_ok,
        error: Some(if parent_ok {
            format!("included in {}", parent.display())
        } else {
            format!(
                "not deleted: {} could not be moved to the trash",
                parent.display()
            )
        }),
    }
}

// ---- tools -----------------------------------------------------------------

#[tool_router]
impl CleanMcp {
    pub fn new(cfg: Config, opts: &McpOptions) -> Result<Self> {
        let roots = opts
            .roots
            .iter()
            .map(|r| {
                cleanmcp_core::resolve_path(r)
                    .map_err(|e| anyhow::anyhow!("--root {}: {e}", r.display()))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut tool_router = Self::tool_router();
        if !opts.allow_delete {
            tool_router.remove_route("move_to_trash");
        }
        Ok(Self {
            cache: Default::default(),
            rules: Arc::new(cfg.rule_set().map_err(anyhow::Error::msg)?),
            exclude: Arc::new(cfg.exclude),
            roots: Arc::new(roots),
            seen: Default::default(),
            tool_router,
        })
    }

    #[tool(
        description = "List drives / mounted volumes with total, used and free space. A good first call.",
        annotations(read_only_hint = true)
    )]
    async fn list_drives(
        &self,
        Parameters(p): Parameters<DrivesParams>,
    ) -> Result<Json<DrivesResult>, String> {
        let all = p.include_virtual.unwrap_or(false);
        let drives = tokio::task::spawn_blocking(move || drives::list_drives(all))
            .await
            .map_err(|e| e.to_string())?;
        Ok(Json(DrivesResult { drives }))
    }

    #[tool(
        description = "Scan a folder or drive and return its size plus its largest direct children. \
                       Results are cached, so call this once on a large root, then drill down with \
                       list_children and the other tools (they reuse the scan). A whole drive can take minutes.",
        annotations(read_only_hint = true)
    )]
    async fn scan_directory(
        &self,
        Parameters(p): Parameters<ScanParams>,
        meta: RequestMetaObject,
        peer: Peer<RoleServer>,
        ct: CancellationToken,
    ) -> Result<Json<ScanSummary>, String> {
        let progress = meta.get_progress_token().map(|t| (t, peer));
        let scan = self
            .get_scan(&p.path, p.refresh, p.one_filesystem, true, progress, ct)
            .await?;
        let abs = self.absolute(&p.path)?;
        let size_mode = mode(p.apparent);
        let s = scan.read().unwrap();
        // A cached scan of a parent folder may have answered; report the folder asked for.
        let t = find(&s, &abs)?;
        let sorted = t.sorted_children(size_mode);
        let top_children = sorted
            .iter()
            .take(15)
            .map(|c| Entry::from_node(&abs.join(c.name.as_str()), c))
            .collect::<Vec<_>>();
        let other = &sorted[top_children.len().min(sorted.len())..];
        self.remember(top_children.iter().map(|e| &e.path));
        Ok(Json(ScanSummary {
            root: abs.clone(),
            disk_size: t.disk_size,
            size: t.size,
            disk_size_human: human(t.disk_size),
            file_count: t.file_count() as u64,
            dir_count: t.dir_count() as u64,
            errors: t.errors() as u64,
            excluded_entries: s.stats.excluded,
            skipped_mounts: s.stats.skipped_mounts,
            hardlinks_deduped: s.stats.hardlinks_deduped,
            cancelled: s.stats.cancelled,
            one_filesystem: s.settings.one_filesystem,
            elapsed_ms: s.elapsed_ms,
            scanned_at_utc: format_time(s.scanned_at),
            top_children,
            other_children_count: other.len(),
            other_children_disk_size: other.iter().map(|c| c.size_by(size_mode)).sum(),
        }))
    }

    #[tool(
        description = "List the children of a folder, largest first (size on disk), optionally several \
                       levels deep. Uses a cached scan if one covers the path. Paginate with offset.",
        annotations(read_only_hint = true)
    )]
    async fn list_children(
        &self,
        Parameters(p): Parameters<ChildrenParams>,
        ct: CancellationToken,
    ) -> Result<Json<TreeJson>, String> {
        let (path, scan) = self.resolve(&p.path, ct).await?;
        let depth = p.depth.unwrap_or(1).clamp(1, 4);
        let top = bounded_top(depth, p.top.unwrap_or(20).min(200));
        let offset = p.offset.unwrap_or(0);
        let size_mode = mode(p.apparent);
        // Building the JSON for a big folder can take a while: keep it off the
        // async workers (and hold the scan lock only inside this blocking task).
        let tree = {
            let scan_path = path.clone();
            tokio::task::spawn_blocking(move || {
                let s = scan.read().unwrap();
                let node = find(&s, &scan_path)?;
                Ok::<_, String>(TreeJson::build_page(
                    node, &scan_path, depth, top, offset, size_mode,
                ))
            })
            .await
            .map_err(|e| e.to_string())??
        };
        let mut paths = Vec::new();
        tree.collect_paths(&mut paths);
        // Only the children count as shown; listing a folder must not make the folder itself deletable.
        self.remember(paths.iter().filter(|p| **p != path));
        Ok(Json(tree))
    }

    #[tool(
        description = "Find the largest files under a folder, with optional filters (minimum size, age, \
                       extensions). Paginate with offset.",
        annotations(read_only_hint = true)
    )]
    async fn largest_files(
        &self,
        Parameters(p): Parameters<FilesParams>,
        ct: CancellationToken,
    ) -> Result<Json<FilesResult>, String> {
        let (path, scan) = self.resolve(&p.path, ct).await?;
        let top = p.top.unwrap_or(25).min(500);
        let offset = p.offset.unwrap_or(0).min(MAX_OFFSET);
        let size_mode = mode(p.apparent);
        let filter = FileFilter {
            min_size: p.min_size_mib.map(|m| m.saturating_mul(1024 * 1024)),
            older_than_days: p.older_than_days,
            newer_than_days: p.newer_than_days,
            extensions: p.extensions,
        };
        // Ask for one extra to know whether there is a next page.
        let wanted = offset.saturating_add(top + 1);
        let all = tokio::task::spawn_blocking(move || {
            let s = scan.read().unwrap();
            let node = find(&s, &path)?;
            Ok::<_, String>(query::largest_files(
                node, &path, wanted, &filter, size_mode,
            ))
        })
        .await
        .map_err(|e| e.to_string())??;
        let (files, next_offset) = page(all, offset, top);
        self.remember(files.iter().map(|e| &e.path));
        Ok(Json(FilesResult { files, next_offset }))
    }

    #[tool(
        description = "Total disk usage per file extension under a folder.",
        annotations(read_only_hint = true)
    )]
    async fn extension_breakdown(
        &self,
        Parameters(p): Parameters<TopParams>,
        ct: CancellationToken,
    ) -> Result<Json<ExtResult>, String> {
        let (path, scan) = self.resolve(&p.path, ct).await?;
        let top = p.top.unwrap_or(25).min(500);
        let size_mode = mode(p.apparent);
        let extensions = tokio::task::spawn_blocking(move || {
            let s = scan.read().unwrap();
            let node = find(&s, &path)?;
            Ok::<_, String>(query::by_extension(node, top, size_mode))
        })
        .await
        .map_err(|e| e.to_string())??;
        Ok(Json(ExtResult { extensions }))
    }

    #[tool(
        description = "How much data under a folder was last modified < 1 month, 1-3 months, ... > 5 years ago. \
                       Old data is a good cleanup target.",
        annotations(read_only_hint = true)
    )]
    async fn age_breakdown(
        &self,
        Parameters(p): Parameters<PathParams>,
        ct: CancellationToken,
    ) -> Result<Json<AgeResult>, String> {
        let (path, scan) = self.resolve(&p.path, ct).await?;
        let buckets = tokio::task::spawn_blocking(move || {
            let s = scan.read().unwrap();
            let node = find(&s, &path)?;
            Ok::<_, String>(query::by_age(node))
        })
        .await
        .map_err(|e| e.to_string())??;
        Ok(Json(AgeResult { buckets }))
    }

    #[tool(
        description = "Suggest folders that are usually safe to delete (build output, dependency folders, \
                       caches, temp files), each with a safety level ('regenerable' or 'review'), a reason \
                       and often a proper cleanup command in 'advice'. Prefer the advice command when given. \
                       Does not delete anything: show the list to the user before removing files.",
        annotations(read_only_hint = true)
    )]
    async fn find_cleanup_candidates(
        &self,
        Parameters(p): Parameters<SuggestParams>,
        ct: CancellationToken,
    ) -> Result<Json<SuggestResult>, String> {
        let (path, scan) = self.resolve(&p.path, ct).await?;
        let rules = self.rules.clone();
        let min_size = mib(p.min_size_mib, 10);
        let size_mode = mode(p.apparent);
        let all = tokio::task::spawn_blocking(move || {
            let s = scan.read().unwrap();
            let node = find(&s, &path)?;
            Ok::<_, String>(rules::find_candidates(
                &rules, node, &path, min_size, size_mode,
            ))
        })
        .await
        .map_err(|e| e.to_string())??;
        let total_disk_size = all.iter().map(|c| c.entry.size_by(size_mode)).sum();
        let total_count = all.len();
        let (candidates, next_offset) = page(
            all,
            p.offset.unwrap_or(0).min(MAX_OFFSET),
            p.top.unwrap_or(30).min(500),
        );
        self.remember(candidates.iter().map(|c| &c.entry.path));
        Ok(Json(SuggestResult {
            total_disk_size,
            total_disk_size_human: human(total_disk_size),
            total_count,
            candidates,
            next_offset,
            note: "Suggestions only. 'regenerable' items are rebuilt or re-downloaded by tools; \
                   'review' items may hold data the user wants, so ask first.",
        }))
    }

    #[tool(
        description = "Find duplicate files (identical content) under a folder, most wasted space first. \
                       Reads file contents, so it can take a while on big folders.",
        annotations(read_only_hint = true)
    )]
    async fn find_duplicates(
        &self,
        Parameters(p): Parameters<DupesParams>,
        ct: CancellationToken,
    ) -> Result<Json<DupesResult>, String> {
        let (path, scan) = self.resolve(&p.path, ct).await?;
        let min_size = mib(p.min_size_mib, 1).max(1);
        let top = p.top.unwrap_or(20).min(200);
        let offset = p.offset.unwrap_or(0).min(MAX_OFFSET);
        let size_mode = mode(p.apparent);
        let groups = tokio::task::spawn_blocking(move || {
            // Hold the scan lock only while listing files, not while reading them.
            let same_size = {
                let s = scan.read().unwrap();
                let node = find(&s, &path)?;
                dupes::same_size_files(node, &path, &dupes::DupeOptions { min_size })
            };
            Ok::<_, String>(dupes::hash_candidates(same_size))
        })
        .await
        .map_err(|e| e.to_string())??;
        let mut groups = groups;
        groups.sort_unstable_by(|a, b| {
            b.wasted_by(size_mode)
                .cmp(&a.wasted_by(size_mode))
                .then_with(|| a.paths.cmp(&b.paths))
        });
        let total_wasted: u64 = groups.iter().map(|g| g.wasted_by(size_mode)).sum();
        let total_groups = groups.len();
        let (groups, next_offset) = page(groups, offset, top);
        self.remember(groups.iter().flat_map(|g| &g.paths));
        Ok(Json(DupesResult {
            total_wasted,
            total_wasted_human: human(total_wasted),
            total_groups,
            groups,
            next_offset,
        }))
    }

    #[tool(
        description = "Scan a folder and save the result to a snapshot file, so it can be compared \
                       later with `diff_snapshots` (what grew or shrank). The output file must be \
                       inside the folders this server may access.",
        annotations(read_only_hint = true)
    )]
    async fn save_snapshot(
        &self,
        Parameters(p): Parameters<SnapshotParams>,
        ct: CancellationToken,
    ) -> Result<Json<SnapshotResult>, String> {
        // The output file does not exist yet, so canonicalize its parent and
        // join the file name; that keeps the root check on a real folder.
        let raw = Path::new(&p.output);
        if raw.file_name().is_none() {
            return Err(format!("{} is not a file path", raw.display()));
        }
        let parent = self.absolute(&raw.parent().unwrap_or(Path::new(".")).to_string_lossy())?;
        let out = parent.join(raw.file_name().unwrap());
        if out.is_dir() {
            return Err(format!(
                "{} is a directory, not a snapshot file",
                out.display()
            ));
        }
        let scan = self
            .get_scan(&p.path, p.refresh, false, false, None, ct)
            .await?;
        let result = tokio::task::spawn_blocking(move || {
            let s = scan.read().unwrap();
            snapshot::save(&s, &out).map_err(|e| format!("cannot write {}: {e}", out.display()))?;
            Ok::<_, String>(SnapshotResult {
                entries: s.tree.file_count() as u64 + s.tree.dir_count() as u64,
                disk_size: s.tree.disk_size,
                disk_size_human: human(s.tree.disk_size),
                scanned_at_utc: format_time(s.scanned_at),
                root: s.root.clone(),
                output: out,
            })
        })
        .await
        .map_err(|e| e.to_string())??;
        Ok(Json(result))
    }

    #[tool(
        description = "Compare a snapshot with a newer snapshot (or a folder scanned now, or the \
                       snapshot's own root rescanned) and report what grew or shrank, largest \
                       absolute change first.",
        annotations(read_only_hint = true)
    )]
    async fn diff_snapshots(
        &self,
        Parameters(p): Parameters<DiffParams>,
        ct: CancellationToken,
    ) -> Result<Json<DiffResult>, String> {
        let old_path = self.absolute(&p.old)?;
        if old_path.is_dir() {
            return Err(format!(
                "{} is a directory, not a snapshot file",
                old_path.display()
            ));
        }
        let depth = p.depth.unwrap_or(3).clamp(1, 6);
        let top = p.top.unwrap_or(10).min(200);
        let size_mode = mode(p.apparent);
        let header = snapshot::read_header(&old_path)
            .map_err(|e| format!("cannot read {}: {e}", old_path.display()))?;
        let (old_scan, new_scan) = match &p.new {
            Some(n) => {
                let new_path = self.absolute(n)?;
                if new_path.is_file() {
                    let o = snapshot::load_truncated(&old_path, depth)
                        .map_err(|e| format!("cannot read {}: {e}", old_path.display()))?;
                    let n = snapshot::load_truncated(&new_path, depth)
                        .map_err(|e| format!("cannot read {}: {e}", new_path.display()))?;
                    (o, n)
                } else if new_path.is_dir() {
                    let o = snapshot::load_truncated(&old_path, depth)
                        .map_err(|e| format!("cannot read {}: {e}", old_path.display()))?;
                    let n = self.get_scan(n, true, false, false, None, ct).await?;
                    let sub = {
                        let n = n.read().unwrap();
                        // The cached scan may cover a parent; diff against the asked folder.
                        let node = find(&n, &new_path)?;
                        Scan {
                            root: new_path,
                            tree: node.clone(),
                            scanned_at: n.scanned_at,
                            started_at: n.started_at,
                            elapsed_ms: n.elapsed_ms,
                            stats: n.stats.clone(),
                            settings: n.settings.clone(),
                        }
                    };
                    (o, sub)
                } else {
                    return Err(format!(
                        "{} is not a snapshot file or directory",
                        new_path.display()
                    ));
                }
            }
            None => {
                let root = header.root.clone();
                if !root.is_dir() {
                    return Err(format!(
                        "the snapshot's root {} no longer exists; pass `new` explicitly",
                        root.display()
                    ));
                }
                let o = snapshot::load_truncated(&old_path, depth)
                    .map_err(|e| format!("cannot read {}: {e}", old_path.display()))?;
                let n = self
                    .get_scan(&root.to_string_lossy(), true, false, false, None, ct)
                    .await?;
                let sub = {
                    let n = n.read().unwrap();
                    let node = find(&n, &root)?;
                    Scan {
                        root,
                        tree: node.clone(),
                        scanned_at: n.scanned_at,
                        started_at: n.started_at,
                        elapsed_ms: n.elapsed_ms,
                        stats: n.stats.clone(),
                        settings: n.settings.clone(),
                    }
                };
                (o, sub)
            }
        };
        let new_root = new_scan.root.clone();
        let diff_base = new_root.clone();
        let changes = tokio::task::spawn_blocking(move || {
            snapshot::diff(
                &old_scan.tree,
                &new_scan.tree,
                &diff_base,
                depth,
                top,
                size_mode,
            )
        })
        .await
        .map_err(|e| e.to_string())?;
        Ok(Json(DiffResult {
            old_root: old_scan.root.clone(),
            new_root,
            old_scanned_at_utc: format_time(old_scan.scanned_at),
            new_scanned_at_utc: format_time(new_scan.scanned_at),
            changes,
        }))
    }

    #[tool(
        description = "Move files or folders to the Recycle Bin / Trash (recoverable; never permanent). \
                       Only paths returned by another tool in this session are accepted, and system \
                       folders, drive roots and the user's home folder are always refused. \
                       Only call this after the user has approved the exact list.",
        annotations(
            destructive_hint = true,
            idempotent_hint = false,
            read_only_hint = false
        )
    )]
    async fn move_to_trash(
        &self,
        Parameters(p): Parameters<TrashParams>,
    ) -> Result<Json<TrashResult>, String> {
        if p.paths.is_empty() {
            return Err("no paths given".into());
        }
        check_trash_request(&p.paths, p.reason.as_deref())?;
        let mut accepted = Vec::new();
        let mut results = Vec::new();
        for raw in &p.paths {
            let refuse = |e: String| Removal {
                path: PathBuf::from(raw),
                ok: false,
                error: Some(e),
            };
            let target = match remove::deletion_target(Path::new(raw)) {
                Ok(t) => t,
                Err(e) => {
                    results.push(refuse(e));
                    continue;
                }
            };
            if let Err(e) = self.delete_allowed(&target) {
                results.push(refuse(e));
            } else {
                accepted.push(target);
            }
        }
        // A path inside another accepted folder goes with it; do not count it
        // twice. Every kept parent is checked, not just the previous path: an
        // unrelated sibling can sort between a folder and its child.
        let (accepted, covered) = prune_covered(accepted);
        // Sizes from the cache, before the nodes are removed.
        let sizes: Vec<u64> = accepted.iter().map(|p| self.cached_disk_size(p)).collect();
        let to_delete = accepted.clone();
        let this = self.clone();
        let removed = tokio::task::spawn_blocking(move || {
            remove::move_to_trash_checked(&to_delete, &|p| this.delete_allowed(p))
        })
        .await
        .map_err(|e| e.to_string())?;
        let mut freed = 0;
        let mut gone = Vec::new();
        for (r, size) in removed.iter().zip(sizes) {
            if r.ok {
                freed += size;
                gone.push(r.path.clone());
            }
            audit(r, size, p.reason.as_deref());
        }
        // A path covered by a parent is only deleted if that parent really moved.
        for (child, parent) in covered {
            let parent_ok = removed
                .iter()
                .find(|r| same_path(&r.path, &parent))
                .is_some_and(|r| r.ok);
            results.push(covered_result(child, &parent, parent_ok));
        }
        // Updating cached scans may wait on long readers; keep it off the async workers.
        let this = self.clone();
        tokio::task::spawn_blocking(move || gone.iter().for_each(|p| this.forget(p)))
            .await
            .map_err(|e| e.to_string())?;
        results.extend(removed);
        Ok(Json(TrashResult {
            results,
            freed_disk_size: freed,
            freed_human: human(freed),
        }))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for CleanMcp {
    fn get_info(&self) -> ServerConfig {
        let mut instructions = String::from(
            "Disk usage explorer for finding what to clean up. Typical flow: list_drives -> \
             scan_directory on a drive or home folder (cached) -> list_children to drill into big \
             folders -> find_cleanup_candidates, largest_files (e.g. older_than_days=180), \
             find_duplicates, age_breakdown. Sizes are bytes on disk. Report findings with sizes and \
             let the user decide; prefer the 'advice' cleanup commands when given.",
        );
        if self.tool_router.has_route("move_to_trash") {
            instructions.push_str(
                " move_to_trash is enabled: use it only for paths the user explicitly approved.",
            );
        } else {
            instructions.push_str(" All tools are read-only; this server cannot delete anything.");
        }
        if !self.roots.is_empty() {
            let roots: Vec<String> = self.roots.iter().map(|r| r.display().to_string()).collect();
            instructions.push_str(&format!(
                " Only these folders are accessible: {}.",
                roots.join(", ")
            ));
        }
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("cleanmcp", env!("CARGO_PKG_VERSION")))
        .with_instructions(instructions)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let r = Resource::new(SCANS_URI, "scans")
            .with_title("Cached scans")
            .with_description("Folders scanned in this session, with totals and scan time")
            .with_mime_type("application/json");
        Ok(ListResourcesResult::with_all_items(vec![r]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        if request.uri != SCANS_URI {
            return Err(ErrorData::resource_not_found(
                format!("unknown resource {}", request.uri),
                None,
            ));
        }
        let scans: Vec<serde_json::Value> = self
            .cache
            .lock()
            .unwrap()
            .iter()
            .map(|s| {
                let s = s.read().unwrap();
                serde_json::json!({
                    "root": s.root,
                    "disk_size": s.tree.disk_size,
                    "disk_size_human": human(s.tree.disk_size),
                    "file_count": s.tree.file_count(),
                    "dir_count": s.tree.dir_count(),
                    "scanned_at_utc": format_time(s.scanned_at),
                    "elapsed_ms": s.elapsed_ms,
                })
            })
            .collect();
        let text = serde_json::to_string_pretty(&scans).unwrap_or_default();
        Ok(ReadResourceResult::new(vec![ResourceContents::text(text, SCANS_URI)]).into())
    }
}

// ---- cache, paths, audit ---------------------------------------------------

impl CleanMcp {
    fn check_root(&self, path: &Path) -> Result<(), String> {
        if self.roots.is_empty() || self.roots.iter().any(|r| is_within(path, r)) {
            Ok(())
        } else {
            Err(format!(
                "{} is outside the folders this server may access",
                path.display()
            ))
        }
    }

    fn absolute(&self, path: &str) -> Result<PathBuf, String> {
        if path.len() > MAX_PATH_LEN {
            return Err(format!(
                "path is too long ({} bytes, max {MAX_PATH_LEN})",
                path.len()
            ));
        }
        let p = cleanmcp_core::resolve_path(Path::new(path))
            .map_err(|e| format!("bad path {path}: {e}"))?;
        self.check_root(&p)?;
        Ok(p)
    }

    fn remember<'a>(&self, paths: impl Iterator<Item = &'a PathBuf>) {
        let mut seen = self.seen.lock().unwrap();
        for path in paths {
            let key = path_key(path);
            if seen.len() >= MAX_SEEN_PATHS && !seen.contains_key(&key) {
                continue;
            }
            seen.insert(key, path.clone());
        }
    }

    /// Return a cached scan covering `path`, or scan it now.
    async fn resolve(
        &self,
        path: &str,
        ct: CancellationToken,
    ) -> Result<(PathBuf, SharedScan), String> {
        let abs = self.absolute(path)?;
        let scan = self.get_scan(path, false, false, false, None, ct).await?;
        Ok((abs, scan))
    }

    async fn get_scan(
        &self,
        path: &str,
        refresh: bool,
        one_filesystem: bool,
        // `scan_directory` answers the scope question the caller asked, so a
        // cached scan made with different settings must not answer it.
        require_settings: bool,
        progress: Option<(rmcp::model::ProgressToken, Peer<RoleServer>)>,
        ct: CancellationToken,
    ) -> Result<SharedScan, String> {
        let abs = self.absolute(path)?;
        if !refresh {
            let mut cache = self.cache.lock().unwrap();
            // The most specific cached root wins: a refreshed subtree must not
            // be shadowed by an older scan of one of its parents.
            let hit = cache
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    let s = s.read().unwrap();
                    is_within(&abs, &s.root)
                        && (!require_settings || s.settings.one_filesystem == one_filesystem)
                })
                .max_by_key(|(_, s)| s.read().unwrap().root.as_os_str().len())
                .map(|(i, _)| i);
            if let Some(i) = hit {
                // Re-insert at the back so eviction drops the least recently used.
                let hit = cache.remove(i);
                cache.push(hit.clone());
                return Ok(hit);
            }
        }
        let opts = ScanOptions {
            exclude: self.exclude.to_vec(),
            one_filesystem,
            ..Default::default()
        };
        let control = opts.control.clone();
        // If this request is dropped before the scan finishes, stop the walker.
        let _cancel_on_drop = CancelOnDrop(control.clone());
        let target = abs.clone();
        let mut job = tokio::task::spawn_blocking(move || cleanmcp_core::scan(&target, &opts));
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        let result = loop {
            tokio::select! {
                r = &mut job => break r,
                _ = ct.cancelled(), if !control.is_cancelled() => control.cancel(),
                _ = tick.tick() => {
                    if let Some((token, peer)) = &progress {
                        let n = control.entries.load(Relaxed);
                        let msg = format!("{n} entries, {}", human(control.bytes.load(Relaxed)));
                        let _ = peer
                            .notify_progress(ProgressNotificationParam::new(token.clone(), n as f64).with_message(msg))
                            .await;
                    }
                }
            }
        };
        let scan = result
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("cannot scan {}: {e}", abs.display()))?;
        if scan.stats.cancelled {
            return Err("scan cancelled".into());
        }
        let root = scan.root.clone();
        let scan_started = scan.started_at;
        let scan = Arc::new(RwLock::new(scan));
        let mut cache = self.cache.lock().unwrap();
        // Drop scans this one replaces (same root, or a nested scan that started
        // before this one), and the oldest if full.
        cache.retain(|s| {
            let s = s.read().unwrap();
            // Keep a nested scan that started after this one did: it is a newer
            // view of that subtree even if it finished first.
            !is_within(&s.root, &root) || s.started_at >= scan_started
        });
        if cache.len() >= MAX_CACHED_SCANS {
            cache.remove(0);
        }
        cache.push(scan.clone());
        Ok(scan)
    }

    /// The allow-list checks, applied when a request arrives and again to the
    /// exact path handed to the trash crate (its parent may have been swapped).
    fn delete_allowed(&self, target: &Path) -> Result<(), String> {
        self.check_root(target)?;
        if self.roots.iter().any(|r| same_path(r, target)) {
            return Err("refused: it is one of the server's --root folders".into());
        }
        let key = path_key(target);
        if !self.seen.lock().unwrap().contains_key(&key) {
            return Err(
                "refused: not returned by a previous tool call in this session; find it \
                        with list_children, largest_files or find_cleanup_candidates first"
                    .into(),
            );
        }
        Ok(())
    }

    /// Disk size of `path` from the most specific scan that contains it.
    fn cached_disk_size(&self, path: &Path) -> u64 {
        // Clone the list first: a scan lock must never be taken under the cache lock.
        let scans: Vec<SharedScan> = self.cache.lock().unwrap().clone();
        scans
            .iter()
            .filter_map(|s| {
                let s = s.read().unwrap();
                let node = s.find(path)?;
                is_within(path, &s.root).then_some((s.root.as_os_str().len(), node.disk_size))
            })
            .max_by_key(|(root_len, _)| *root_len)
            .map_or(0, |(_, size)| size)
    }

    /// Same scan cache and settings, fresh "seen paths" list (one per HTTP session).
    fn new_session(&self) -> Self {
        Self {
            seen: Default::default(),
            ..self.clone()
        }
    }

    /// Remove a trashed path from every cached scan and from the seen list.
    /// Blocking: may wait for readers of the scans.
    fn forget(&self, path: &Path) {
        // Do not hold the cache lock while waiting for a scan's write lock.
        let scans: Vec<SharedScan> = self.cache.lock().unwrap().clone();
        for s in scans.iter() {
            let mut s = s.write().unwrap();
            if is_within(path, &s.root) {
                s.remove(path);
            }
        }
        self.seen.lock().unwrap().retain(|_, p| !is_within(p, path));
    }
}

fn find<'a>(scan: &'a Scan, path: &Path) -> Result<&'a Node, String> {
    scan.find(path).ok_or_else(|| {
        format!(
            "{} not found in scan of {}",
            path.display(),
            scan.root.display()
        )
    })
}

/// `$CLEANMCP_DATA_DIR`, else `%LOCALAPPDATA%\cleanmcp` or `~/.local/share/cleanmcp`.
pub fn data_dir() -> Option<PathBuf> {
    std::env::var_os("CLEANMCP_DATA_DIR")
        .map(PathBuf::from)
        .or_else(cleanmcp_core::config::data_dir)
}

/// Append one line per deletion attempt to the audit log and stderr.
fn audit(r: &Removal, disk_size: u64, reason: Option<&str>) {
    tracing::info!(path = %r.path.display(), ok = r.ok, error = ?r.error, "move_to_trash");
    let Some(dir) = data_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let line = serde_json::json!({
        "time_utc": format_time(cleanmcp_core::scan::unix_now()),
        "path": r.path.to_string_lossy(),
        "ok": r.ok,
        "error": r.error,
        "disk_size": disk_size,
        "reason": reason,
    });
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("deletions.log"))
    {
        let _ = writeln!(f, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paging() {
        let (items, next) = page((0..10).collect::<Vec<_>>(), 0, 4);
        assert_eq!(items, [0, 1, 2, 3]);
        assert_eq!(next, Some(4));
        let (items, next) = page((0..10).collect::<Vec<_>>(), 8, 4);
        assert_eq!(items, [8, 9]);
        assert_eq!(next, None);
        let (items, next) = page((0..3).collect::<Vec<_>>(), 10, 4);
        assert!(items.is_empty());
        assert_eq!(next, None);
    }

    #[test]
    fn fan_out_is_bounded_by_depth() {
        // One level keeps the caller's top...
        assert_eq!(bounded_top(1, 200), 200);
        // ...deeper levels are lowered so depth ÃƒÆ’Ã¢â‚¬â€ top stays within the budget.
        assert_eq!(bounded_top(2, 200), 141);
        assert_eq!(bounded_top(3, 200), 27);
        assert_eq!(bounded_top(4, 200), 11);
        assert!(bounded_top(4, 200).pow(4) <= MAX_TREE_ROWS);
        assert_eq!(bounded_top(2, 5), 5);
    }

    #[test]
    fn delete_tool_is_opt_in() {
        let ro = CleanMcp::new(Config::default(), &McpOptions::default()).unwrap();
        assert!(!ro.tool_router.has_route("move_to_trash"));
        assert!(
            ro.tool_router.list_all().iter().all(|t| t
                .annotations
                .as_ref()
                .and_then(|a| a.read_only_hint)
                == Some(true))
        );
        let rw = CleanMcp::new(
            Config::default(),
            &McpOptions {
                allow_delete: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(rw.tool_router.has_route("move_to_trash"));
    }

    #[test]
    fn snapshot_tools_are_registered_read_only() {
        let ro = CleanMcp::new(Config::default(), &McpOptions::default()).unwrap();
        for name in ["save_snapshot", "diff_snapshots"] {
            let all = ro.tool_router.list_all();
            let t = all
                .iter()
                .find(|t| t.name == name)
                .unwrap_or_else(|| panic!("{name} not registered"));
            assert_eq!(
                t.annotations.as_ref().and_then(|a| a.read_only_hint),
                Some(true),
                "{name} must stay read-only"
            );
        }
        // The apparent-size option deserializes on every tool that has it.
        let p: FilesParams = serde_json::from_str(r#"{"path": "/tmp", "apparent": true}"#).unwrap();
        assert_eq!(mode(p.apparent), SizeMode::Apparent);
        let p: DupesParams = serde_json::from_str(r#"{"path": "/tmp"}"#).unwrap();
        assert_eq!(mode(p.apparent), SizeMode::Disk);
    }

    #[test]
    fn roots_are_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let s = CleanMcp::new(
            Config::default(),
            &McpOptions {
                roots: vec![dir.path().to_path_buf()],
                ..Default::default()
            },
        )
        .unwrap();
        assert!(s.absolute(&dir.path().to_string_lossy()).is_ok());
        let outside = dir.path().parent().unwrap().to_string_lossy().into_owned();
        assert!(s.absolute(&outside).is_err());
        // `..` tricks are resolved before the check.
        let sneaky = format!("{}/..", dir.path().display());
        assert!(s.absolute(&sneaky).is_err());
        // An absurdly long string is refused before it reaches the filesystem.
        let huge = "x".repeat(MAX_PATH_LEN + 1);
        assert!(s.absolute(&huge).is_err());
    }

    fn delete_server(root: &Path) -> CleanMcp {
        CleanMcp::new(
            Config::default(),
            &McpOptions {
                roots: vec![root.to_path_buf()],
                allow_delete: true,
                http: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn trash_requests_are_bounded() {
        let paths: Vec<String> = (0..=MAX_TRASH_PATHS)
            .map(|i| format!("/tmp/f{i}"))
            .collect();
        let err = check_trash_request(&paths, None).unwrap_err();
        assert!(err.contains("too many paths"), "{err}");

        let long = "x".repeat(MAX_PATH_LEN + 1);
        let err = check_trash_request(&[long], None).unwrap_err();
        assert!(err.contains("too long"), "{err}");

        let reason = "r".repeat(MAX_REASON_LEN + 1);
        let err = check_trash_request(&["/tmp/a".into()], Some(&reason)).unwrap_err();
        assert!(err.contains("reason is too long"), "{err}");

        assert!(check_trash_request(&["/tmp/a".into()], Some("old build output")).is_ok());
    }

    #[test]
    fn nested_targets_are_pruned_and_reported_honestly() {
        let accepted = ["/a", "/a-b", "/a/b", "/a/b/c"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let (top, covered) = prune_covered(accepted);
        // `/a/b` sorts after `/a-b`, but is still inside `/a`.
        assert_eq!(top, [PathBuf::from("/a"), PathBuf::from("/a-b")]);
        assert_eq!(covered.len(), 2, "{covered:?}");

        let parent = Path::new("/a");
        assert!(covered_result(PathBuf::from("/a/b"), parent, true).ok);
        let failed = covered_result(PathBuf::from("/a/b"), parent, false);
        assert!(!failed.ok);
        assert!(failed.error.unwrap().contains("not deleted"));
    }

    #[test]
    fn seen_paths_are_bounded() {
        let s = CleanMcp::new(Config::default(), &McpOptions::default()).unwrap();
        let paths: Vec<PathBuf> = (0..MAX_SEEN_PATHS + 5)
            .map(|i| PathBuf::from(format!("/p/{i}")))
            .collect();
        s.remember(paths.iter());
        assert_eq!(s.seen.lock().unwrap().len(), MAX_SEEN_PATHS);
        // Already remembered paths can still be refreshed after the cap is hit.
        s.remember(paths.iter().take(1));
        assert_eq!(s.seen.lock().unwrap().len(), MAX_SEEN_PATHS);
    }

    #[cfg(windows)]
    #[test]
    fn shown_paths_match_in_any_case() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Project")).unwrap();
        let file = dir.path().join("Project").join("Report.TXT");
        std::fs::write(&file, b"x").unwrap();
        let root = cleanmcp_core::resolve_path(dir.path()).unwrap();
        let canonical = cleanmcp_core::resolve_path(&file).unwrap();
        let s = delete_server(&root);
        s.remember(std::iter::once(&canonical));
        // The same file, retyped in another case, is recognised as shown.
        let variant = canonical.with_file_name("report.txt");
        assert!(s.delete_allowed(&variant).is_ok());
    }

    /// The case-insensitive seen-set must not let a case variant of a `--root`
    /// folder itself through the root guard.
    #[cfg(windows)]
    #[test]
    fn root_folder_is_refused_even_with_a_different_case() {
        let dir = tempfile::tempdir().unwrap();
        let root = cleanmcp_core::resolve_path(dir.path()).unwrap();
        let s = delete_server(&root);
        s.remember(std::iter::once(&root));
        let name = root.file_name().unwrap().to_string_lossy().to_uppercase();
        let variant = root.with_file_name(&name);
        assert_eq!(
            path_key(&root),
            path_key(&variant),
            "test needs a case variant"
        );
        let err = s.delete_allowed(&variant).unwrap_err();
        assert!(err.contains("--root"), "{err}");
    }

    #[cfg(not(windows))]
    #[test]
    fn shown_paths_are_case_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Report.TXT");
        std::fs::write(&file, b"x").unwrap();
        let root = cleanmcp_core::resolve_path(dir.path()).unwrap();
        let canonical = cleanmcp_core::resolve_path(&file).unwrap();
        let s = delete_server(&root);
        s.remember(std::iter::once(&canonical));
        assert!(s.delete_allowed(&canonical).is_ok());
        let variant = canonical.with_file_name("report.txt");
        assert!(s.delete_allowed(&variant).is_err());
    }
}
