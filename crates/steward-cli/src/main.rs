use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use steward_proto::{Entry, Request, Response};

#[derive(Parser)]
#[command(name = "steward", about = "Query and drive the steward file index")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Indexed roots and daemon state.
    Status,
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
    /// Send a raw JSON request.
    Raw {
        json: String,
    },
}

struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect() -> Result<Self> {
        let path = steward_proto::socket_path();
        let writer = UnixStream::connect(&path)
            .with_context(|| format!("connecting to stewardd at {}", path.display()))?;
        Ok(Self {
            reader: BufReader::new(writer.try_clone()?),
            writer,
        })
    }

    fn call(&mut self, req: &Value) -> Result<Value> {
        let mut line = serde_json::to_vec(req)?;
        line.push(b'\n');
        self.writer.write_all(&line)?;
        let mut buf = String::new();
        self.reader.read_line(&mut buf)?;
        match serde_json::from_str(&buf)? {
            Response::Ok { result } => Ok(result),
            Response::Error { message } => bail!("{message}"),
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

fn tree(c: &mut Client, e: &Entry, depth: u32, top: usize, indent: usize) -> Result<()> {
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
    let mut c = Client::connect()?;
    let out = match cli.cmd {
        Cmd::Status => c.request(&Request::Status)?,
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
        Cmd::Raw { json: text } => {
            c.call(&serde_json::from_str::<Value>(&text).unwrap_or(json!(text)))?
        }
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
