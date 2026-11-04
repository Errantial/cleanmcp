mod mcp;
mod output;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use cleanmcp_core::{ScanOptions, query, rules, scan};

/// TreeSize-style disk usage explorer with a built-in MCP server for AI agents.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show a size tree of a directory (largest first).
    Tree {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// How many levels to show.
        #[arg(short, long, default_value_t = 2)]
        depth: usize,
        /// Children to show per directory.
        #[arg(short = 'n', long, default_value_t = 10)]
        top: usize,
        #[arg(long)]
        json: bool,
    },
    /// List the largest files under a directory.
    Files {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(short = 'n', long, default_value_t = 20)]
        top: usize,
        /// Only files not modified for at least this many days.
        #[arg(long)]
        older_than_days: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Break down disk usage by file extension.
    Ext {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(short = 'n', long, default_value_t = 20)]
        top: usize,
        #[arg(long)]
        json: bool,
    },
    /// Suggest directories that are usually safe to clean (build output, caches, ...).
    Suggest {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Ignore matches smaller than this many MiB.
        #[arg(long, default_value_t = 10)]
        min_mib: u64,
        #[arg(long)]
        json: bool,
    },
    /// Run as an MCP server over stdio so agents can query disk usage.
    Mcp,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Mcp => {
            // stdout carries the MCP protocol, so logs must go to stderr.
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
                .init();
            tokio::runtime::Runtime::new()?.block_on(mcp::serve())
        }
        Command::Tree {
            path,
            depth,
            top,
            json,
        } => {
            let s = do_scan(&path)?;
            if json {
                output::print_json(&output::TreeJson::build(&s.tree, &s.root, depth, top))
            } else {
                output::print_tree(&s, depth, top);
                Ok(())
            }
        }
        Command::Files {
            path,
            top,
            older_than_days,
            json,
        } => {
            let s = do_scan(&path)?;
            let min_age = older_than_days.map(|d| Duration::from_secs(d * 86_400));
            let files = query::largest_files(&s.tree, &s.root, top, min_age);
            if json {
                output::print_json(&files)
            } else {
                output::print_entries(&files);
                Ok(())
            }
        }
        Command::Ext { path, top, json } => {
            let s = do_scan(&path)?;
            let stats = query::by_extension(&s.tree, top);
            if json {
                output::print_json(&stats)
            } else {
                output::print_extensions(&stats, s.tree.size);
                Ok(())
            }
        }
        Command::Suggest {
            path,
            min_mib,
            json,
        } => {
            let s = do_scan(&path)?;
            let c = rules::find_candidates(&s.tree, &s.root, min_mib * 1024 * 1024);
            if json {
                output::print_json(&c)
            } else {
                output::print_candidates(&c);
                Ok(())
            }
        }
    }
}

fn do_scan(path: &std::path::Path) -> Result<cleanmcp_core::Scan> {
    scan(path, &ScanOptions::default()).with_context(|| format!("cannot scan {}", path.display()))
}
