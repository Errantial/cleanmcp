mod mcp;
mod output;
mod progress;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use cleanmcp_core::{Scan, ScanOptions, SizeMode, query, rules};

/// TreeSize-style disk usage explorer with a built-in MCP server for AI agents.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Options shared by every command that scans.
#[derive(Args, Clone)]
pub struct ScanArgs {
    /// Directory (or drive, e.g. `C:\`) to scan.
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// Report logical file sizes instead of space used on disk.
    #[arg(long)]
    pub apparent: bool,
    /// Skip glob patterns (name or full path). Repeatable: `-e node_modules -e '**/*.iso'`.
    #[arg(short, long = "exclude", value_name = "GLOB")]
    pub exclude: Vec<String>,
    /// Stay on the filesystem of PATH (Linux; Windows never crosses mount points).
    #[arg(short = 'x', long)]
    pub one_filesystem: bool,
    /// Also scan /proc, /sys, /dev and /run (Linux).
    #[arg(long)]
    pub include_virtual: bool,
    /// Count hard-linked files once on Windows (slower). Always on for Linux.
    #[arg(long)]
    pub dedupe_hardlinks: bool,
    /// Do not show the progress line on stderr.
    #[arg(short, long)]
    pub quiet: bool,
}

impl ScanArgs {
    fn mode(&self) -> SizeMode {
        if self.apparent {
            SizeMode::Apparent
        } else {
            SizeMode::Disk
        }
    }

    fn options(&self) -> ScanOptions {
        ScanOptions {
            one_filesystem: self.one_filesystem,
            include_virtual: self.include_virtual,
            dedupe_hardlinks: self.dedupe_hardlinks,
            exclude: self.exclude.clone(),
            ..Default::default()
        }
    }

    fn run(&self) -> Result<Scan> {
        let opts = self.options();
        let reporter = (!self.quiet).then(|| progress::Reporter::start(opts.control.clone()));
        let result = cleanmcp_core::scan(&self.path, &opts);
        drop(reporter);
        result.with_context(|| format!("cannot scan {}", self.path.display()))
    }
}

#[derive(Subcommand)]
enum Command {
    /// Show a size tree of a directory (largest first).
    Tree {
        #[command(flatten)]
        scan: ScanArgs,
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
        #[command(flatten)]
        scan: ScanArgs,
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
        #[command(flatten)]
        scan: ScanArgs,
        #[arg(short = 'n', long, default_value_t = 20)]
        top: usize,
        #[arg(long)]
        json: bool,
    },
    /// Suggest directories that are usually safe to clean (build output, caches, ...).
    Suggest {
        #[command(flatten)]
        scan: ScanArgs,
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
            scan,
            depth,
            top,
            json,
        } => {
            let s = scan.run()?;
            if json {
                output::print_json(&output::TreeJson::build(
                    &s.tree,
                    &s.root,
                    depth,
                    top,
                    scan.mode(),
                ))
            } else {
                output::print_tree(&s, depth, top, scan.mode());
                Ok(())
            }
        }
        Command::Files {
            scan,
            top,
            older_than_days,
            json,
        } => {
            let s = scan.run()?;
            let min_age = older_than_days.map(|d| Duration::from_secs(d * 86_400));
            let files = query::largest_files(&s.tree, &s.root, top, min_age, scan.mode());
            if json {
                output::print_json(&files)
            } else {
                output::print_entries(&files, scan.mode());
                Ok(())
            }
        }
        Command::Ext { scan, top, json } => {
            let s = scan.run()?;
            let stats = query::by_extension(&s.tree, top, scan.mode());
            if json {
                output::print_json(&stats)
            } else {
                output::print_extensions(&stats, &s.tree, scan.mode());
                Ok(())
            }
        }
        Command::Suggest {
            scan,
            min_mib,
            json,
        } => {
            let s = scan.run()?;
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
