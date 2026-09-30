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
    RemoveRoot {
        path: PathBuf,
    },
    /// Content-id coverage and duplication under PATH.
    ContentSummary {
        path: PathBuf,
    },
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
    Invalidate {
        path: PathBuf,
    },
    /// Children of a directory, largest first.
    Ls {
        path: PathBuf,
    },
    /// qdirstat-style tree, largest first.
    Tree {
        path: PathBuf,
        #[arg(short, long, default_value_t = 2)]
        depth: u32,
        #[arg(short, long, default_value_t = 10)]
        top: usize,
    },
    Stat {
        path: PathBuf,
    },
    /// Find entries by name: substring, or glob if it has * ? [.
    Locate {
        pattern: String,
        #[arg(short, long, default_value_t = 1000)]
        limit: u32,
    },
    Classify {
        path: PathBuf,
    },
    /// BitTorrent v2 content id of a file.
    Cid {
        path: PathBuf,
    },
    /// Compute missing content ids under a path.
    Hash {
        path: PathBuf,
    },
    /// Paths whose content has this id.
    Find {
        id: String,
    },
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
    /// Send a raw JSON request.
    Raw {
        json: String,
    },
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

fn line(e: &Entry, parent_alloc: u64, indent: usize) {
    let pct = if parent_alloc == 0 {
        0.0
    } else {
        e.total_alloc as f64 * 100.0 / parent_alloc as f64
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
    println!(
        "{:>8} {:>5.1}% {:<10} {:>8} {}{}{}  {}",
        human(e.total_alloc),
        pct,
        bar,
        e.total_files,
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

    fn call(&mut self, req: &Value) -> Result<Value> {
        match self {
            Self::Socket(c) => Ok(c.call(req)?),
            Self::Local(rt, engine) => {
                let req: Request = serde_json::from_value(req.clone())?;
                rt.block_on(engine.handle(req))
            }
        }
    }

    fn request(&mut self, req: &Request) -> Result<Value> {
        self.call(&serde_json::to_value(req)?)
    }

    fn children(&mut self, path: PathBuf) -> Result<Vec<Entry>> {
        Ok(serde_json::from_value(
            self.request(&Request::Children { path })?,
        )?)
    }
}

fn tree(c: &mut Backend, e: &Entry, depth: u32, top: usize, indent: usize) -> Result<()> {
    if depth == 0 || !matches!(e.kind, steward_proto::Kind::Dir) {
        return Ok(());
    }
    let kids = c.children(PathBuf::from(&e.path))?;
    let rest = kids.len().saturating_sub(top);
    for k in kids.iter().take(top) {
        line(k, e.total_alloc, indent);
        tree(c, k, depth - 1, top, indent + 1)?;
    }
    if rest > 0 {
        let bytes: u64 = kids.iter().skip(top).map(|k| k.total_alloc).sum();
        println!(
            "{:>8} {:>18} {}… {rest} more",
            human(bytes),
            "",
            "  ".repeat(indent)
        );
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
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
        Cmd::Ls { path } => {
            let path = abs(path)?;
            let me: Entry =
                serde_json::from_value(c.request(&Request::Stat { path: path.clone() })?)?;
            for k in c.children(path)? {
                line(&k, me.total_alloc, 0);
            }
            return Ok(());
        }
        Cmd::Tree { path, depth, top } => {
            let me: Entry =
                serde_json::from_value(c.request(&Request::Stat { path: abs(path)? })?)?;
            line(&me, me.total_alloc, 0);
            return tree(&mut c, &me, depth, top, 1);
        }
        Cmd::Stat { path } => c.request(&Request::Stat { path: abs(path)? })?,
        Cmd::Locate { pattern, limit } => {
            for p in c
                .request(&Request::Locate { pattern, limit })?
                .as_array()
                .into_iter()
                .flatten()
            {
                println!("{}", p.as_str().unwrap_or_default());
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
        Cmd::Raw { json: text } => {
            c.call(&serde_json::from_str::<Value>(&text).unwrap_or(json!(text)))?
        }
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
