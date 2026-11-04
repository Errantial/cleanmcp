//! MCP server (stdio). Exposes read-only disk usage tools to AI agents.
//!
//! Scans are cached in memory per server process, so an agent can scan a drive
//! once and then drill down with `list_children` without rescanning.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use cleanmcp_core::query::{self, Entry, ExtensionStat};
use cleanmcp_core::rules::{self, Candidate};
use cleanmcp_core::{Node, Scan, ScanOptions, SizeMode};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{Json, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::output::TreeJson;

pub async fn serve() -> Result<()> {
    let service = CleanMcp::new().serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[derive(Clone)]
pub struct CleanMcp {
    cache: Arc<Mutex<Vec<Arc<Scan>>>>,
    tool_router: ToolRouter<Self>,
}

// ---- tool parameters -------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct ScanParams {
    /// Directory to scan, e.g. "C:\\" or "/home/me".
    pub path: String,
    /// Rescan even if a cached scan covers this path.
    #[serde(default)]
    pub refresh: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct ChildrenParams {
    pub path: String,
    /// Levels to expand (default 1).
    pub depth: Option<usize>,
    /// Children to list per directory (default 20).
    pub top: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct FilesParams {
    pub path: String,
    /// How many files to return (default 25).
    pub top: Option<usize>,
    /// Only files not modified for at least this many days.
    pub older_than_days: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ExtParams {
    pub path: String,
    pub top: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SuggestParams {
    pub path: String,
    /// Ignore matches smaller than this many MiB (default 10).
    pub min_mib: Option<u64>,
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
    pub top_children: Vec<Entry>,
}

#[derive(Serialize, JsonSchema)]
pub struct FilesResult {
    pub files: Vec<Entry>,
}

#[derive(Serialize, JsonSchema)]
pub struct ExtResult {
    pub extensions: Vec<ExtensionStat>,
}

#[derive(Serialize, JsonSchema)]
pub struct SuggestResult {
    /// Sum of the candidates' on-disk sizes.
    pub total_disk_size: u64,
    pub total_disk_size_human: String,
    pub candidates: Vec<Candidate>,
    pub note: &'static str,
}

// ---- tools -----------------------------------------------------------------

#[tool_router]
impl CleanMcp {
    pub fn new() -> Self {
        Self {
            cache: Default::default(),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Scan a directory tree and return its total size plus its largest direct children. \
                       Results are cached; call this first, then drill down with list_children.",
        annotations(read_only_hint = true)
    )]
    async fn scan_directory(
        &self,
        Parameters(p): Parameters<ScanParams>,
    ) -> Result<Json<ScanSummary>, String> {
        let scan = self.get_scan(&p.path, p.refresh).await?;
        let t = &scan.tree;
        let top_children = t
            .children
            .iter()
            .take(15)
            .map(|c| Entry::from_node(&scan.root.join(&*c.name), c))
            .collect();
        Ok(Json(ScanSummary {
            root: scan.root.clone(),
            disk_size: t.disk_size,
            size: t.size,
            disk_size_human: crate::output::human(t.disk_size),
            file_count: t.file_count as u64,
            dir_count: t.dir_count as u64,
            errors: t.errors as u64,
            elapsed_ms: scan.elapsed_ms,
            top_children,
        }))
    }

    #[tool(
        description = "List the children of a directory sorted by size, optionally several levels deep. \
                       Uses a cached scan if one covers the path.",
        annotations(read_only_hint = true)
    )]
    async fn list_children(
        &self,
        Parameters(p): Parameters<ChildrenParams>,
    ) -> Result<Json<TreeJson>, String> {
        let (path, scan) = self.resolve(&p.path).await?;
        let node = find(&scan, &path)?;
        Ok(Json(TreeJson::build(
            node,
            &path,
            p.depth.unwrap_or(1),
            p.top.unwrap_or(20),
            SizeMode::Disk,
        )))
    }

    #[tool(
        description = "Find the largest files under a directory.",
        annotations(read_only_hint = true)
    )]
    async fn largest_files(
        &self,
        Parameters(p): Parameters<FilesParams>,
    ) -> Result<Json<FilesResult>, String> {
        let (path, scan) = self.resolve(&p.path).await?;
        let node = find(&scan, &path)?;
        let min_age = p.older_than_days.map(|d| Duration::from_secs(d * 86_400));
        Ok(Json(FilesResult {
            files: query::largest_files(node, &path, p.top.unwrap_or(25), min_age, SizeMode::Disk),
        }))
    }

    #[tool(
        description = "Total disk usage per file extension under a directory.",
        annotations(read_only_hint = true)
    )]
    async fn extension_breakdown(
        &self,
        Parameters(p): Parameters<ExtParams>,
    ) -> Result<Json<ExtResult>, String> {
        let (path, scan) = self.resolve(&p.path).await?;
        let node = find(&scan, &path)?;
        Ok(Json(ExtResult {
            extensions: query::by_extension(node, p.top.unwrap_or(25), SizeMode::Disk),
        }))
    }

    #[tool(
        description = "Suggest directories that are usually safe to delete (build output, dependency \
                       folders, caches), each with a safety level and reason. Does not delete anything; \
                       show the list to the user before removing files.",
        annotations(read_only_hint = true)
    )]
    async fn find_cleanup_candidates(
        &self,
        Parameters(p): Parameters<SuggestParams>,
    ) -> Result<Json<SuggestResult>, String> {
        let (path, scan) = self.resolve(&p.path).await?;
        let node = find(&scan, &path)?;
        let candidates = rules::find_candidates(node, &path, p.min_mib.unwrap_or(10) * 1024 * 1024);
        let total_disk_size = candidates.iter().map(|c| c.entry.disk_size).sum();
        Ok(Json(SuggestResult {
            total_disk_size,
            total_disk_size_human: crate::output::human(total_disk_size),
            candidates,
            note: "Suggestions only. 'regenerable' items are rebuilt by tools; 'review' items may hold user data.",
        }))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for CleanMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("cleanmcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Disk usage explorer. Start with scan_directory on a drive or folder, then use \
                 list_children, largest_files, extension_breakdown and find_cleanup_candidates to \
                 work out what can be cleaned up. All tools are read-only.",
            )
    }
}

// ---- cache -----------------------------------------------------------------

impl CleanMcp {
    /// Return a cached scan covering `path`, or scan it now.
    async fn resolve(&self, path: &str) -> Result<(PathBuf, Arc<Scan>), String> {
        let abs = absolute(path)?;
        let scan = self.get_scan(path, false).await?;
        Ok((abs, scan))
    }

    async fn get_scan(&self, path: &str, refresh: bool) -> Result<Arc<Scan>, String> {
        let abs = absolute(path)?;
        if !refresh {
            let cache = self.cache.lock().unwrap();
            if let Some(s) = cache.iter().find(|s| abs.starts_with(&s.root)) {
                return Ok(s.clone());
            }
        }
        let target = abs.clone();
        let scan = tokio::task::spawn_blocking(move || {
            cleanmcp_core::scan(&target, &ScanOptions::default())
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("cannot scan {}: {e}", abs.display()))?;
        let scan = Arc::new(scan);
        let mut cache = self.cache.lock().unwrap();
        // Drop scans this one replaces (same root or nested inside it).
        cache.retain(|s| !s.root.starts_with(&scan.root));
        cache.push(scan.clone());
        Ok(scan)
    }
}

fn absolute(path: &str) -> Result<PathBuf, String> {
    cleanmcp_core::resolve_path(Path::new(path)).map_err(|e| format!("bad path {path}: {e}"))
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
