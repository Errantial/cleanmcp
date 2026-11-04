//! Core disk-usage engine shared by the CLI and the MCP server.
//!
//! - [`scan`] walks a directory in parallel and builds an in-memory [`Node`] tree.
//! - [`query`] answers questions about a tree (largest files, extensions, ...).
//! - [`rules`] flags directories that are usually safe to clean up.
//!
//! Nothing in this crate deletes files.

pub mod query;
pub mod rules;
pub mod scan;
pub mod tree;

pub use scan::{ScanOptions, scan};
pub use tree::{Node, NodeKind, Scan};
