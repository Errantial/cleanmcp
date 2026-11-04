mod mcp;
mod output;
mod progress;
mod remove;
mod ui;

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use cleanmcp_core::config::Config;
use cleanmcp_core::query::FileFilter;
use cleanmcp_core::{Scan, ScanOptions, SizeMode, drives, dupes, query, rules, snapshot};

/// TreeSize-style disk usage explorer with a built-in MCP server for AI agents.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Config file (default: %APPDATA%\cleanmcp\config.toml or ~/.config/cleanmcp/config.toml).
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
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

    fn run(&self, cfg: &Config) -> Result<Scan> {
        let opts = ScanOptions {
            one_filesystem: self.one_filesystem,
            include_virtual: self.include_virtual,
            dedupe_hardlinks: self.dedupe_hardlinks,
            exclude: self.exclude.iter().chain(&cfg.exclude).cloned().collect(),
            ..Default::default()
        };
        run_scan(&self.path, opts, self.quiet)
    }
}

/// Scan with an optional progress line, then print notes (errors, exclusions...) to stderr.
fn run_scan(path: &std::path::Path, opts: ScanOptions, quiet: bool) -> Result<Scan> {
    let reporter = (!quiet).then(|| progress::Reporter::start(opts.control.clone()));
    let result = cleanmcp_core::scan(path, &opts);
    drop(reporter);
    let scan = result.with_context(|| format!("cannot scan {}", path.display()))?;
    output::print_scan_notes(&scan);
    if root_unreadable(&scan) {
        anyhow::bail!(
            "cannot read {}: the folder could not be listed (permission denied?)",
            path.display()
        );
    }
    Ok(scan)
}

/// A folder that exists but could not be listed at all. Reporting it as "0 B"
/// with a success exit code would be a lie, and `--json` callers never see the
/// stderr note.
fn root_unreadable(scan: &Scan) -> bool {
    scan.tree.is_dir() && scan.tree.children().is_empty() && scan.tree.errors() > 0
}

/// Parse sizes like `500MB`, `1.5GiB`, `4096`.
fn parse_size(s: &str) -> Result<u64, String> {
    s.parse::<bytesize::ByteSize>()
        .map(|b| b.as_u64())
        .map_err(|e| e.to_string())
}

#[derive(Clone, Copy, ValueEnum)]
enum ExportFormat {
    Csv,
    Json,
}

#[derive(Subcommand)]
enum Command {
    /// Browse interactively: navigate, mark and move items to the trash.
    Ui {
        #[command(flatten)]
        scan: ScanArgs,
    },
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
    /// List the largest files, with optional size/age/type filters.
    Files {
        #[command(flatten)]
        scan: ScanArgs,
        #[arg(short = 'n', long, default_value_t = 20)]
        top: usize,
        /// Only files at least this big, e.g. `100MB`.
        #[arg(long, value_parser = parse_size, value_name = "SIZE")]
        min_size: Option<u64>,
        /// Only files not modified for at least this many days.
        #[arg(long, value_name = "DAYS")]
        older_than: Option<u64>,
        /// Only files modified less than this many days ago.
        #[arg(long, value_name = "DAYS")]
        newer_than: Option<u64>,
        /// Only these extensions, e.g. `--ext iso,zip`.
        #[arg(long, value_delimiter = ',')]
        ext: Vec<String>,
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
    /// Break down disk usage by file age (last modified).
    Age {
        #[command(flatten)]
        scan: ScanArgs,
        #[arg(long)]
        json: bool,
    },
    /// Find duplicate files (same content).
    Dupes {
        #[command(flatten)]
        scan: ScanArgs,
        /// Ignore files smaller than this.
        #[arg(long, value_parser = parse_size, default_value = "1MiB", value_name = "SIZE")]
        min_size: u64,
        /// Groups to show.
        #[arg(short = 'n', long, default_value_t = 20)]
        top: usize,
        #[arg(long)]
        json: bool,
    },
    /// Suggest directories that are usually safe to clean (build output, caches, ...).
    Suggest {
        #[command(flatten)]
        scan: ScanArgs,
        /// Ignore matches smaller than this.
        #[arg(long, value_parser = parse_size, default_value = "10MiB", value_name = "SIZE")]
        min_size: u64,
        #[arg(long)]
        json: bool,
    },
    /// List the cleanup rules in effect (built-in + config file).
    Rules {
        #[arg(long)]
        json: bool,
    },
    /// List drives / mounted volumes with free space.
    Drives {
        /// Include pseudo filesystems (tmpfs, squashfs, ...).
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Export every file and directory as CSV or JSON.
    Export {
        #[command(flatten)]
        scan: ScanArgs,
        #[arg(short, long, value_enum, default_value = "csv")]
        format: ExportFormat,
        /// Output file (default: stdout).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Directories only.
        #[arg(long)]
        dirs_only: bool,
        /// Skip entries smaller than this.
        #[arg(long, value_parser = parse_size, value_name = "SIZE")]
        min_size: Option<u64>,
    },
    /// Save a scan to a file for a later `diff` (`.gz` = compressed).
    Snapshot {
        #[command(flatten)]
        scan: ScanArgs,
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Show what grew or shrank between a snapshot and now (or another snapshot).
    Diff {
        /// Snapshot written by `cleanmcp snapshot`.
        old: PathBuf,
        /// Newer snapshot, or a directory to scan now. Default: rescan the old snapshot's root.
        new: Option<PathBuf>,
        #[arg(short, long, default_value_t = 3)]
        depth: usize,
        #[arg(short = 'n', long, default_value_t = 10)]
        top: usize,
        #[arg(long)]
        apparent: bool,
        /// Do not show the progress line when rescanning.
        #[arg(short, long)]
        quiet: bool,
        #[arg(long)]
        json: bool,
    },
    /// Print a shell completion script, e.g. `cleanmcp completions bash > ~/.local/share/bash-completion/completions/cleanmcp`.
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Print the man page (roff) to stdout.
    #[command(hide = true)]
    Man,
    /// Run as an MCP server (stdio by default) so AI agents can explore disk usage.
    Mcp {
        /// Only allow access inside this folder. Repeatable. Default: anywhere.
        #[arg(long = "root", value_name = "DIR")]
        roots: Vec<PathBuf>,
        /// Add the move_to_trash tool (recoverable, refuses system folders).
        #[arg(long)]
        allow_delete: bool,
        /// Serve over HTTP at ADDR (e.g. 127.0.0.1:8765) instead of stdio.
        #[arg(long, value_name = "ADDR")]
        http: Option<std::net::SocketAddr>,
    },
}

/// Load the config, treating a missing *explicit* path as a typo rather than
/// "use defaults" (the default location may legitimately not exist).
fn load_config(path: Option<&std::path::Path>) -> Result<Config> {
    if let Some(p) = path
        && !p.exists()
    {
        bail!("config file not found: {}", p.display());
    }
    Config::load(path).map_err(|e| anyhow::anyhow!("config error: {e}"))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.clone();
    // Loaded only by commands that use it, so a broken config cannot block the others.
    let config = || load_config(config_path.as_deref());
    match cli.command {
        Command::Completions { shell } => {
            clap_complete::generate(
                shell,
                &mut Cli::command(),
                "cleanmcp",
                &mut std::io::stdout(),
            );
            Ok(())
        }
        Command::Man => {
            clap_mangen::Man::new(Cli::command()).render(&mut std::io::stdout())?;
            Ok(())
        }
        Command::Mcp {
            roots,
            allow_delete,
            http,
        } => {
            // stdout carries the MCP protocol, so logs must go to stderr.
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
                .init();
            // A broken config must not take the agent's tools down: warn and use defaults.
            let cfg = config().unwrap_or_else(|e| {
                tracing::warn!("ignoring config file: {e}");
                Config::default()
            });
            let opts = mcp::McpOptions {
                roots,
                allow_delete,
                http,
            };
            tokio::runtime::Runtime::new()?.block_on(mcp::serve(cfg, opts))
        }
        Command::Drives { all, json } => {
            let d = drives::list_drives(all);
            if json {
                output::print_json(&d)
            } else {
                output::print_drives(&d);
                Ok(())
            }
        }

        Command::Ui { scan } => {
            let cfg = config()?;
            let s = scan.run(&cfg)?;
            ui::run(s, scan.mode())
        }
        Command::Tree {
            scan,
            depth,
            top,
            json,
        } => {
            let cfg = config()?;
            let s = scan.run(&cfg)?;
            let mode = scan.mode();
            if json {
                output::print_json(&output::TreeJson::build(&s.tree, &s.root, depth, top, mode))
            } else {
                output::print_tree(&s, depth, top, mode);
                Ok(())
            }
        }
        Command::Files {
            scan,
            top,
            min_size,
            older_than,
            newer_than,
            ext,
            json,
        } => {
            let cfg = config()?;
            let s = scan.run(&cfg)?;
            let filter = FileFilter {
                min_size,
                older_than_days: older_than,
                newer_than_days: newer_than,
                extensions: ext,
            };
            let files = query::largest_files(&s.tree, &s.root, top, &filter, scan.mode());
            if json {
                output::print_json(&files)
            } else {
                output::print_entries(&files, scan.mode());
                Ok(())
            }
        }
        Command::Ext { scan, top, json } => {
            let cfg = config()?;
            let s = scan.run(&cfg)?;
            let stats = query::by_extension(&s.tree, top, scan.mode());
            if json {
                output::print_json(&stats)
            } else {
                output::print_extensions(&stats, &s.tree, scan.mode());
                Ok(())
            }
        }
        Command::Age { scan, json } => {
            let cfg = config()?;
            let s = scan.run(&cfg)?;
            let stats = query::by_age(&s.tree);
            if json {
                output::print_json(&stats)
            } else {
                output::print_age(&stats, scan.mode());
                Ok(())
            }
        }
        Command::Dupes {
            scan,
            min_size,
            top,
            json,
        } => {
            let cfg = config()?;
            let s = scan.run(&cfg)?;
            let mut groups =
                dupes::find_duplicates(&s.tree, &s.root, &dupes::DupeOptions { min_size });
            let total_wasted: u64 = groups.iter().map(|g| g.wasted).sum();
            let total_groups = groups.len();
            groups.truncate(top);
            if json {
                output::print_json(&groups)
            } else {
                output::print_dupes(&groups, total_groups, total_wasted);
                Ok(())
            }
        }
        Command::Suggest {
            scan,
            min_size,
            json,
        } => {
            let cfg = config()?;
            let rules = cfg.rule_set().map_err(anyhow::Error::msg)?;
            let s = scan.run(&cfg)?;
            let c = rules::find_candidates(&rules, &s.tree, &s.root, min_size, scan.mode());
            if json {
                output::print_json(&c)
            } else {
                output::print_candidates(&c, scan.mode());
                Ok(())
            }
        }
        Command::Rules { json } => {
            let cfg = config()?;
            let rules = cfg.rule_set().map_err(anyhow::Error::msg)?;
            if json {
                output::print_json(rules.rules())
            } else {
                output::print_rules(rules.rules());
                Ok(())
            }
        }
        Command::Export {
            scan,
            format,
            output: out,
            dirs_only,
            min_size,
        } => {
            let cfg = config()?;
            let s = scan.run(&cfg)?;
            let mut w: Box<dyn Write> = match &out {
                Some(p) => Box::new(std::io::BufWriter::new(
                    std::fs::File::create(p)
                        .with_context(|| format!("cannot create {}", p.display()))?,
                )),
                None => Box::new(std::io::BufWriter::new(std::io::stdout().lock())),
            };
            let rows = output::ExportOptions {
                dirs_only,
                min_size: min_size.unwrap_or(0),
                mode: scan.mode(),
            };
            match format {
                ExportFormat::Csv => output::export_csv(&s, &rows, &mut w)?,
                ExportFormat::Json => output::export_json(&s, &rows, &mut w)?,
            }
            w.flush()?;
            Ok(())
        }
        Command::Snapshot { scan, output: out } => {
            let cfg = config()?;
            let s = scan.run(&cfg)?;
            snapshot::save(&s, &out).with_context(|| format!("cannot write {}", out.display()))?;
            eprintln!(
                "Saved {} entries ({} on disk) to {}",
                s.tree.file_count() as u64 + s.tree.dir_count() as u64,
                output::human(s.tree.disk_size),
                out.display()
            );
            Ok(())
        }
        Command::Diff {
            old,
            new,
            depth,
            top,
            apparent,
            quiet,
            json,
        } => {
            let cfg = config()?;
            let header = snapshot::read_header(&old)
                .with_context(|| format!("cannot read {}", old.display()))?;
            let mode = if apparent {
                SizeMode::Apparent
            } else {
                SizeMode::Disk
            };
            // Only the first `depth` levels are ever looked at, so the snapshot
            // is loaded truncated; that keeps the second tree small. When the
            // other side has to be rescanned, loading and scanning run at the
            // same time.
            let (old_scan, new_scan) = match new {
                Some(p) if p.is_file() => (
                    snapshot::load_truncated(&old, depth)
                        .with_context(|| format!("cannot read {}", old.display()))?,
                    snapshot::load_truncated(&p, depth)
                        .with_context(|| format!("cannot read {}", p.display()))?,
                ),
                other => {
                    let path = other.unwrap_or_else(|| header.root.clone());
                    if !path.is_dir() {
                        bail!("{} is not a snapshot file or directory", path.display());
                    }
                    let mut opts = ScanOptions::from_settings(&header.settings);
                    for e in &cfg.exclude {
                        if !opts.exclude.contains(e) {
                            opts.exclude.push(e.clone());
                        }
                    }
                    std::thread::scope(|scope| {
                        let loader = scope.spawn(|| snapshot::load_truncated(&old, depth));
                        let new_scan = run_scan(&path, opts, quiet)?;
                        let old_scan = loader
                            .join()
                            .map_err(|_| anyhow::anyhow!("snapshot loader thread panicked"))?
                            .with_context(|| format!("cannot read {}", old.display()))?;
                        Ok::<_, anyhow::Error>((old_scan, new_scan))
                    })?
                }
            };
            if old_scan.root != new_scan.root {
                eprintln!(
                    "warning: comparing different folders ({} vs {}); the result shows how they differ, not what changed",
                    old_scan.root.display(),
                    new_scan.root.display()
                );
            }
            if old_scan.settings != new_scan.settings {
                eprintln!(
                    "warning: the two scans used different options (exclude, --one-filesystem, ...)"
                );
            }
            let d = snapshot::diff(
                &old_scan.tree,
                &new_scan.tree,
                &new_scan.root,
                depth,
                top,
                mode,
            );
            if json {
                output::print_json(&d)
            } else {
                output::print_diff(&d, old_scan.scanned_at, new_scan.scanned_at);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cleanmcp_core::{Node, NodeKind};

    fn scan_with(tree: Node) -> Scan {
        Scan {
            root: "d".into(),
            tree,
            scanned_at: 0,
            started_at: 0,
            elapsed_ms: 0,
            stats: Default::default(),
            settings: Default::default(),
        }
    }

    #[test]
    fn unreadable_root_is_detected() {
        let mut empty = Node::new(NodeKind::Dir, "d".into(), 0, 0, 0);
        assert!(!root_unreadable(&scan_with(empty.clone())));

        // Listing failed: one error and no children.
        empty.add_error();
        assert!(root_unreadable(&scan_with(empty)));

        // A file root is never "unreadable" in this sense.
        let f = Node::new(NodeKind::File, "f".into(), 1, 1, 0);
        assert!(!root_unreadable(&scan_with(f)));
    }

    #[test]
    fn explicit_missing_config_is_an_error() {
        let missing = std::path::Path::new("no-such-cleanmcp-config.toml");
        assert!(!missing.exists());
        let err = load_config(Some(missing)).unwrap_err().to_string();
        assert!(err.contains("config file not found"), "{err}");

        // A path that does exist still loads (an empty file is valid defaults).
        let dir = tempfile::tempdir().unwrap();
        let ok = dir.path().join("c.toml");
        std::fs::write(&ok, "").unwrap();
        assert!(load_config(Some(&ok)).is_ok());
    }
}
