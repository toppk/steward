use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use steward_proto::{Client, Entry, Request};
use stewardd::config::Config;
use stewardd::engine::Engine;

#[derive(Parser)]
#[command(name = "steward", about = "Query and drive the steward file index")]
struct Cli {
    /// Work directly on this index file instead of through stewardd.
    #[arg(long, global = true, env = "STEWARD_DB")]
    db: Option<PathBuf>,
    /// Log more to stderr: -v info, -vv debug, -vvv trace (RUST_LOG overrides).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Indexed roots and daemon state.
    Status,
    /// Re-read settings.toml: add new roots, prune removed ones.
    Reload,
    /// Configured roots, their policies and index state.
    Settings,
    /// Add a root (or replace the one at PATH) in settings.toml and apply it.
    PutRoot {
        path: PathBuf,
        #[arg(long, default_value_t = steward_proto::default_interval())]
        interval: u64,
        #[arg(long, default_value_t = steward_proto::default_full_every())]
        full_every: u32,
        /// Also scan other filesystems mounted below PATH.
        #[arg(long)]
        cross_filesystems: bool,
        #[arg(long)]
        no_classify: bool,
        /// gitignore-style pattern relative to PATH; repeatable.
        #[arg(long)]
        exclude: Vec<String>,
        /// Folder (relative to PATH) whose files get content ids; repeatable.
        #[arg(long)]
        contentid: Vec<PathBuf>,
    },
    /// Remove a root from settings.toml and the index.
    RemoveRoot { path: PathBuf },
    /// Content-id coverage and duplication under PATH.
    ContentSummary { path: PathBuf },
    /// BEP-52 piece layer of a content id (hex), from the stored 1 MiB layer.
    PieceLayer {
        id: String,
        /// Bytes; a power of two of at least 1 MiB.
        #[arg(short, long, default_value_t = 1 << 20)]
        piece_size: u64,
    },
    /// Rescan a path now.
    Scan {
        path: PathBuf,
        /// Skip reading directories whose mtime is unchanged.
        #[arg(long)]
        trust: bool,
    },
    /// Tell the daemon something under PATH changed.
    Invalidate { path: PathBuf },
    /// Children of a directory, largest first.
    Ls {
        path: PathBuf,
        /// Rank by space on disk, or by items (entries beneath: roughly inodes).
        #[arg(short, long, value_enum, default_value_t = By::Space)]
        by: By,
    },
    /// qdirstat-style tree, largest first.
    Tree {
        path: PathBuf,
        #[arg(short, long, default_value_t = 2)]
        depth: u32,
        #[arg(short, long, default_value_t = 10)]
        top: usize,
        /// Rank by space on disk, or by items (entries beneath: roughly inodes).
        #[arg(short, long, value_enum, default_value_t = By::Space)]
        by: By,
    },
    /// One entry: its stat fields, subtree totals, tags and content id.
    Stat { path: PathBuf },
    /// Find entries by name: substring, or glob if it has * ? [.
    Locate {
        pattern: String,
        #[arg(short, long, default_value_t = 1000)]
        limit: u32,
    },
    /// Re-run classification (repositories, build output, caches…) under PATH.
    Classify { path: PathBuf },
    /// BitTorrent v2 content id of a file.
    Cid { path: PathBuf },
    /// Compute missing content ids under a path.
    Hash { path: PathBuf },
    /// Paths whose content has this id.
    Find { id: String },
    /// Duplicate content under a path, most wasted space first.
    Dups {
        path: PathBuf,
        #[arg(short, long, default_value_t = 50)]
        limit: u32,
    },
    /// Write a qdirstat 2.0 cache file (gzip) for PATH.
    ExportQdirstat {
        path: PathBuf,
        #[arg(short, long)]
        out: PathBuf,
    },
    /// Where copies of each content id are now, and whether they are reachable.
    Resolve {
        ids: Vec<String>,
        /// Re-stat each copy before answering.
        #[arg(long)]
        recheck: bool,
    },
    /// Update the index for these paths now and give their content ids.
    Inspect { paths: Vec<PathBuf> },
    /// Print content and storage events as they happen (one JSON per line).
    Events {
        /// Replay the daemon's backlog after this sequence number first.
        #[arg(long)]
        since: Option<u64>,
        /// Only content events for these ids (storage events always show).
        #[arg(long)]
        id: Vec<String>,
    },
    /// Call any method: `steward raw stat '{"path": "/tmp"}'`.
    Raw {
        method: String,
        params: Option<String>,
    },
}

/// `println!` that ends the program quietly once stdout's reader has gone
/// (`steward locate x | head`), instead of panicking.
macro_rules! out {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        if writeln!(std::io::stdout(), $($arg)*).is_err() {
            std::process::exit(0);
        }
    }};
}

fn abs(p: PathBuf) -> Result<PathBuf> {
    Ok(std::path::absolute(p)?)
}

fn human(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n}B")
    } else {
        format!("{v:.1}{}", UNITS[u])
    }
}

fn name(e: &Entry) -> &str {
    e.path.rsplit('/').next().unwrap_or(&e.path)
}

/// What `tree` and `ls` rank by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum By {
    /// Space on disk.
    Space,
    /// Entries beneath: files, directories, symlinks and the rest. Roughly
    /// the inodes (or, on btrfs, the metadata) a subtree uses.
    Items,
}

impl By {
    fn of(self, e: &Entry) -> u64 {
        match self {
            Self::Space => e.total_alloc,
            Self::Items => e.total_items,
        }
    }

    fn show(self, n: u64) -> String {
        match self {
            Self::Space => human(n),
            Self::Items => count(n),
        }
    }
}

/// An item count in at most 6 characters: 438, 9512, 41.7k, 15.2M.
fn count(n: u64) -> String {
    match n {
        0..10_000 => n.to_string(),
        10_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{:.1}G", n as f64 / 1e9),
    }
}

fn line(e: &Entry, parent: &Entry, by: By, indent: usize) {
    let whole = by.of(parent);
    let pct = if whole == 0 {
        0.0
    } else {
        by.of(e) as f64 * 100.0 / whole as f64
    };
    let bar = "#".repeat((pct / 10.0).round() as usize);
    let slash = if matches!(e.kind, steward_proto::Kind::Dir) {
        "/"
    } else {
        ""
    };
    let mut extra = e.tags.join(" ");
    if let Some(c) = &e.category {
        extra.push_str(c);
    }
    // The other measure goes in the last column.
    let other = match by {
        By::Space => e.total_files.to_string(),
        By::Items => human(e.total_alloc),
    };
    out!(
        "{:>8} {:>5.1}% {:<10} {:>8} {}{}{}  {}",
        by.show(by.of(e)),
        pct,
        bar,
        other,
        "  ".repeat(indent),
        name(e),
        slash,
        extra
    );
}

/// The daemon over its socket, or the same engine in this process.
enum Backend {
    Socket(Client),
    Local(tokio::runtime::Runtime, Box<Engine>),
}

impl Backend {
    fn open(db: Option<PathBuf>) -> Result<Self> {
        let Some(db) = db else {
            return Ok(Self::Socket(Client::connect()?));
        };
        let rt = tokio::runtime::Runtime::new()?;
        let mut config = Config::load()?;
        config.db = std::path::absolute(db)?;
        let engine = rt.block_on(Engine::open(config))?;
        Ok(Self::Local(rt, Box::new(engine)))
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        match self {
            Self::Socket(c) => Ok(c.call(method, params)?),
            Self::Local(rt, engine) => {
                let req = steward_proto::RpcRequest {
                    jsonrpc: steward_proto::JSONRPC.into(),
                    id: None,
                    method: method.into(),
                    params: Some(params),
                }
                .request()
                .map_err(|e| anyhow::anyhow!(e.message))?;
                rt.block_on(engine.handle(req))
            }
        }
    }

    fn request(&mut self, req: &Request) -> Result<Value> {
        let v = serde_json::to_value(req)?;
        let method = v["method"].as_str().unwrap_or_default().to_string();
        self.call(&method, v.get("params").cloned().unwrap_or(json!({})))
    }

    fn children(&mut self, path: PathBuf) -> Result<Vec<Entry>> {
        Ok(serde_json::from_value(
            self.request(&Request::Children { path })?,
        )?)
    }
}

fn tree(c: &mut Backend, e: &Entry, depth: u32, top: usize, by: By, indent: usize) -> Result<()> {
    if depth == 0 || !matches!(e.kind, steward_proto::Kind::Dir) {
        return Ok(());
    }
    let mut kids = c.children(PathBuf::from(&e.path))?;
    kids.sort_by_key(|k| std::cmp::Reverse(by.of(k)));
    let rest = kids.len().saturating_sub(top);
    for k in kids.iter().take(top) {
        line(k, e, by, indent);
        tree(c, k, depth - 1, top, by, indent + 1)?;
    }
    if rest > 0 {
        let sum: u64 = kids.iter().skip(top).map(|k| by.of(k)).sum();
        out!(
            "{:>8} {:>18} {}… {rest} more",
            by.show(sum),
            "",
            "  ".repeat(indent)
        );
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    steward_log::init(steward_log::Level::WARN, cli.verbose, false);
    match run(cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let mut c = Backend::open(cli.db)?;
    let out = match cli.cmd {
        Cmd::Status => c.request(&Request::Status)?,
        Cmd::Reload => c.request(&Request::Reload)?,
        Cmd::Settings => c.request(&Request::Settings)?,
        Cmd::PutRoot {
            path,
            interval,
            full_every,
            cross_filesystems,
            no_classify,
            exclude,
            contentid,
        } => {
            let root = steward_proto::RootSettings {
                path: abs(path)?,
                interval_minutes: interval,
                full_every,
                one_filesystem: !cross_filesystems,
                exclude,
                classify: !no_classify,
                contentid,
            };
            c.request(&Request::PutRoot { root })?
        }
        Cmd::RemoveRoot { path } => c.request(&Request::RemoveRoot { path: abs(path)? })?,
        Cmd::ContentSummary { path } => c.request(&Request::ContentSummary { path: abs(path)? })?,
        Cmd::PieceLayer { id, piece_size } => c.request(&Request::PieceLayer { id, piece_size })?,
        Cmd::Scan { path, trust } => c.request(&Request::Scan {
            path: abs(path)?,
            trust_dir_mtime: trust,
        })?,
        Cmd::Invalidate { path } => c.request(&Request::Invalidate { path: abs(path)? })?,
        Cmd::Ls { path, by } => {
            let path = abs(path)?;
            let me: Entry =
                serde_json::from_value(c.request(&Request::Stat { path: path.clone() })?)?;
            let mut kids = c.children(path)?;
            kids.sort_by_key(|k| std::cmp::Reverse(by.of(k)));
            for k in &kids {
                line(k, &me, by, 0);
            }
            return Ok(());
        }
        Cmd::Tree {
            path,
            depth,
            top,
            by,
        } => {
            let me: Entry =
                serde_json::from_value(c.request(&Request::Stat { path: abs(path)? })?)?;
            line(&me, &me, by, 0);
            return tree(&mut c, &me, depth, top, by, 1);
        }
        Cmd::Stat { path } => c.request(&Request::Stat { path: abs(path)? })?,
        Cmd::Locate { pattern, limit } => {
            for p in c
                .request(&Request::Locate { pattern, limit })?
                .as_array()
                .into_iter()
                .flatten()
            {
                out!("{}", p.as_str().unwrap_or_default());
            }
            return Ok(());
        }
        Cmd::Classify { path } => c.request(&Request::Classify { path: abs(path)? })?,
        Cmd::Cid { path } => c.request(&Request::ContentId { path: abs(path)? })?,
        Cmd::Hash { path } => c.request(&Request::HashTree { path: abs(path)? })?,
        Cmd::Find { id } => c.request(&Request::FindContent { id })?,
        Cmd::Dups { path, limit } => c.request(&Request::Duplicates {
            path: abs(path)?,
            limit,
        })?,
        Cmd::ExportQdirstat { path, out } => c.request(&Request::ExportQdirstat {
            path: abs(path)?,
            out: abs(out)?,
        })?,
        Cmd::Resolve { ids, recheck } => c.request(&Request::Resolve {
            contents: ids
                .into_iter()
                .map(|id| steward_proto::ContentRef { id, size: None })
                .collect(),
            recheck,
        })?,
        Cmd::Inspect { paths } => c.request(&Request::Inspect {
            paths: paths.into_iter().map(abs).collect::<Result<_>>()?,
        })?,
        Cmd::Events { since, id } => {
            let Backend::Socket(client) = c else {
                anyhow::bail!("events come from the daemon; drop --db");
            };
            let ids = (!id.is_empty()).then_some(id);
            let mut stream = client.subscribe(since, ids)?;
            eprintln!("{}", stream.start);
            for msg in &mut stream {
                out!("{}", msg?);
            }
            return Ok(());
        }
        Cmd::Raw { method, params } => {
            let params = match params {
                Some(p) => serde_json::from_str(&p)?,
                None => json!({}),
            };
            c.call(&method, params)?
        }
    };
    out!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
