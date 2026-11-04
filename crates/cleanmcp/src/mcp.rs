//! MCP server: disk usage tools for AI agents, over stdio or HTTP.
//!
//! Scans are cached in memory per server process, so an agent can scan a drive
//! once and then drill down without rescanning. All tools are read-only unless
//! the server is started with `--allow-delete`, which adds `move_to_trash`.

use std::collections::HashSet;
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
    seen: Arc<Mutex<HashSet<PathBuf>>>,
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
}

#[derive(Deserialize, JsonSchema)]
pub struct PathParams {
    pub path: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ChildrenParams {
    pub path: String,
    /// Levels to expand (default 1, max 4).
    pub depth: Option<usize>,
    /// Children per folder (default 20, max 200).
    pub top: Option<usize>,
    /// Skip this many children at the first level (pagination).
    pub offset: Option<usize>,
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
}

#[derive(Deserialize, JsonSchema)]
pub struct TopParams {
    pub path: String,
    /// How many rows (default 25).
    pub top: Option<usize>,
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
}

#[derive(Deserialize, JsonSchema)]
pub struct DupesParams {
    pub path: String,
    /// Ignore files smaller than this many MiB (default 1).
    pub min_size_mib: Option<u64>,
    /// Groups to return (default 20, max 200).
    pub top: Option<usize>,
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
    pub elapsed_ms: u64,
    pub scanned_at_utc: String,
    /// Largest direct children, by size on disk.
    pub top_children: Vec<Entry>,
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

fn mib(v: Option<u64>, default: u64) -> u64 {
    v.unwrap_or(default).saturating_mul(1024 * 1024)
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
    async fn list_drives(&self) -> Result<Json<DrivesResult>, String> {
        let drives = tokio::task::spawn_blocking(|| drives::list_drives(false))
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
            .get_scan(&p.path, p.refresh, p.one_filesystem, progress, ct)
            .await?;
        let abs = self.absolute(&p.path)?;
        let s = scan.read().unwrap();
        // A cached scan of a parent folder may have answered; report the folder asked for.
        let t = find(&s, &abs)?;
        let top_children = t
            .children
            .iter()
            .take(15)
            .map(|c| Entry::from_node(&abs.join(c.name.as_str()), c))
            .collect::<Vec<_>>();
        self.remember(top_children.iter().map(|e| &e.path));
        Ok(Json(ScanSummary {
            root: abs.clone(),
            disk_size: t.disk_size,
            size: t.size,
            disk_size_human: human(t.disk_size),
            file_count: t.file_count as u64,
            dir_count: t.dir_count as u64,
            errors: t.errors as u64,
            elapsed_ms: s.elapsed_ms,
            scanned_at_utc: format_time(s.scanned_at),
            top_children,
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
    ) -> Result<Json<TreeJson>, String> {
        let (path, scan) = self.resolve(&p.path).await?;
        let s = scan.read().unwrap();
        let node = find(&s, &path)?;
        let tree = TreeJson::build_page(
            node,
            &path,
            p.depth.unwrap_or(1).clamp(1, 4),
            p.top.unwrap_or(20).min(200),
            p.offset.unwrap_or(0),
            SizeMode::Disk,
        );
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
    ) -> Result<Json<FilesResult>, String> {
        let (path, scan) = self.resolve(&p.path).await?;
        let s = scan.read().unwrap();
        let node = find(&s, &path)?;
        let top = p.top.unwrap_or(25).min(500);
        let offset = p.offset.unwrap_or(0).min(MAX_OFFSET);
        let filter = FileFilter {
            min_size: p.min_size_mib.map(|m| m.saturating_mul(1024 * 1024)),
            older_than_days: p.older_than_days,
            newer_than_days: p.newer_than_days,
            extensions: p.extensions,
        };
        // Ask for one extra to know whether there is a next page.
        let all = query::largest_files(
            node,
            &path,
            offset.saturating_add(top + 1),
            &filter,
            SizeMode::Disk,
        );
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
    ) -> Result<Json<ExtResult>, String> {
        let (path, scan) = self.resolve(&p.path).await?;
        let s = scan.read().unwrap();
        let node = find(&s, &path)?;
        Ok(Json(ExtResult {
            extensions: query::by_extension(node, p.top.unwrap_or(25).min(500), SizeMode::Disk),
        }))
    }

    #[tool(
        description = "How much data under a folder was last modified < 1 month, 1-3 months, ... > 5 years ago. \
                       Old data is a good cleanup target.",
        annotations(read_only_hint = true)
    )]
    async fn age_breakdown(
        &self,
        Parameters(p): Parameters<PathParams>,
    ) -> Result<Json<AgeResult>, String> {
        let (path, scan) = self.resolve(&p.path).await?;
        let s = scan.read().unwrap();
        let node = find(&s, &path)?;
        Ok(Json(AgeResult {
            buckets: query::by_age(node),
        }))
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
    ) -> Result<Json<SuggestResult>, String> {
        let (path, scan) = self.resolve(&p.path).await?;
        let s = scan.read().unwrap();
        let node = find(&s, &path)?;
        let all = rules::find_candidates(&self.rules, node, &path, mib(p.min_size_mib, 10));
        let total_disk_size = all.iter().map(|c| c.entry.disk_size).sum();
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
    ) -> Result<Json<DupesResult>, String> {
        let (path, scan) = self.resolve(&p.path).await?;
        let min_size = mib(p.min_size_mib, 1).max(1);
        let top = p.top.unwrap_or(20).min(200);
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
        let total_wasted: u64 = groups.iter().map(|g| g.wasted).sum();
        let total_groups = groups.len();
        let groups: Vec<DuplicateGroup> = groups.into_iter().take(top).collect();
        self.remember(groups.iter().flat_map(|g| &g.paths));
        Ok(Json(DupesResult {
            total_wasted,
            total_wasted_human: human(total_wasted),
            total_groups,
            groups,
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
            if let Err(e) = self.check_root(&target) {
                results.push(refuse(e));
            } else if self.roots.contains(&target) {
                results.push(refuse(
                    "refused: it is one of the server's --root folders".into(),
                ));
            } else if !self.seen.lock().unwrap().contains(&target) {
                results.push(refuse(
                    "refused: not returned by a previous tool call in this session; find it with \
                     list_children, largest_files or find_cleanup_candidates first"
                        .into(),
                ));
            } else {
                accepted.push(target);
            }
        }
        // A path inside another accepted folder goes with it; do not count it twice.
        accepted.sort();
        accepted.dedup();
        let mut top_level: Vec<PathBuf> = Vec::new();
        for t in accepted {
            if top_level.last().is_some_and(|last| t.starts_with(last)) {
                results.push(Removal {
                    path: t,
                    ok: true,
                    error: Some("included in a parent folder in this request".into()),
                });
            } else {
                top_level.push(t);
            }
        }
        let accepted = top_level;
        // Sizes from the cache, before the nodes are removed.
        let sizes: Vec<u64> = accepted.iter().map(|p| self.cached_disk_size(p)).collect();
        let to_delete = accepted.clone();
        let removed = tokio::task::spawn_blocking(move || remove::move_to_trash(&to_delete))
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
                    "file_count": s.tree.file_count,
                    "dir_count": s.tree.dir_count,
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
        if self.roots.is_empty() || self.roots.iter().any(|r| path.starts_with(r)) {
            Ok(())
        } else {
            Err(format!(
                "{} is outside the folders this server may access",
                path.display()
            ))
        }
    }

    fn absolute(&self, path: &str) -> Result<PathBuf, String> {
        let p = cleanmcp_core::resolve_path(Path::new(path))
            .map_err(|e| format!("bad path {path}: {e}"))?;
        self.check_root(&p)?;
        Ok(p)
    }

    fn remember<'a>(&self, paths: impl Iterator<Item = &'a PathBuf>) {
        let mut seen = self.seen.lock().unwrap();
        seen.extend(paths.cloned());
    }

    /// Return a cached scan covering `path`, or scan it now.
    async fn resolve(&self, path: &str) -> Result<(PathBuf, SharedScan), String> {
        let abs = self.absolute(path)?;
        let scan = self
            .get_scan(path, false, false, None, CancellationToken::new())
            .await?;
        Ok((abs, scan))
    }

    async fn get_scan(
        &self,
        path: &str,
        refresh: bool,
        one_filesystem: bool,
        progress: Option<(rmcp::model::ProgressToken, Peer<RoleServer>)>,
        ct: CancellationToken,
    ) -> Result<SharedScan, String> {
        let abs = self.absolute(path)?;
        if !refresh {
            let cache = self.cache.lock().unwrap();
            if let Some(s) = cache
                .iter()
                .find(|s| abs.starts_with(&s.read().unwrap().root))
            {
                return Ok(s.clone());
            }
        }
        let opts = ScanOptions {
            exclude: self.exclude.to_vec(),
            one_filesystem,
            ..Default::default()
        };
        let control = opts.control.clone();
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
        let scan = Arc::new(RwLock::new(scan));
        let mut cache = self.cache.lock().unwrap();
        // Drop scans this one replaces (same root or nested inside it), and the oldest if full.
        cache.retain(|s| !s.read().unwrap().root.starts_with(&root));
        if cache.len() >= MAX_CACHED_SCANS {
            cache.remove(0);
        }
        cache.push(scan.clone());
        Ok(scan)
    }

    fn cached_disk_size(&self, path: &Path) -> u64 {
        let cache = self.cache.lock().unwrap();
        cache
            .iter()
            .find_map(|s| s.read().unwrap().find(path).map(|n| n.disk_size))
            .unwrap_or(0)
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
            if path.starts_with(&s.root) {
                s.remove(path);
            }
        }
        self.seen.lock().unwrap().retain(|p| !p.starts_with(path));
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
    }
}
