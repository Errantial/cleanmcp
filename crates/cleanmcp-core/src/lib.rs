//! Core disk-usage engine shared by the CLI and the MCP server.
//!
//! - [`scan`] walks a directory in parallel and builds an in-memory [`Node`] tree.
//! - [`query`] answers questions about a tree (largest files, extensions, ...).
//! - [`rules`] flags directories that are usually safe to clean up.
//!
//! Nothing in this crate deletes files.

mod os;
pub mod query;
pub mod rules;
pub mod scan;
pub mod tree;

pub use scan::{ScanControl, ScanOptions, scan};

/// Resolve a user-supplied path the same way [`scan`] resolves its root
/// (absolute, symlinks and `..` resolved, real letter case on Windows), so it can
/// be looked up in a scan with [`Scan::find`].
pub fn resolve_path(path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    dunce::canonicalize(path)
}
pub use tree::{Node, NodeKind, Scan, ScanStats, SizeMode};
