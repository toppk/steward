//! The steward service: owns the index, keeps it current by periodic rescans
//! and client invalidations (no inotify), and answers queries on a unix
//! socket.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use steward_proto::{Request, Response};
use stewardd::config::Config;
use stewardd::engine::Engine;
use stewardd::say;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

/// Invalidations arriving within this window are coalesced into one rescan.
const INVALIDATE_DEBOUNCE: Duration = Duration::from_secs(2);

#[derive(Parser)]
#[command(name = "stewardd", about = "The steward file index service", version)]
struct Args {
    /// -v: roots, their settings and state changes; -vv: also connections.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    stewardd::log::set_verbosity(args.verbose);
    let (tx, rx) = mpsc::unbounded_channel();
    let (reload_tx, mut reload_rx) = mpsc::unbounded_channel();
    let (hash_tx, mut hash_rx) = mpsc::unbounded_channel::<PathBuf>();
    let config = Config::load()?;
    say!(
        1,
        "settings {}, index {}",
        Config::path().display(),
        config.db.display()
    );
    for r in &config.roots {
        stewardd::engine::describe_root(r);
    }
    let mut engine = Engine::open(config).await?;
    engine.invalidate = Some(tx);
    engine.reloaded = Some(reload_tx);
    engine.hash_queue = Some(hash_tx);
    engine.managed = true;
    let engine = Arc::new(engine);
    engine.prune().await?;

    let socket = steward_proto::socket_path();
    let listener = bind(&socket)?;
    say!(0, "listening on {}", socket.display());

    tokio::spawn(invalidation_worker(Arc::clone(&engine), rx));
    {
        // One hashing job at a time, off the scan lock; queued repeats of a
        // subtree collapse because fresh ids are skipped.
        let engine = Arc::clone(&engine);
        tokio::spawn(async move {
            while let Some(target) = hash_rx.recv().await {
                engine.hash_started(&target);
                let t = std::time::Instant::now();
                let result = engine.hash_tree(&target).await;
                say!(
                    1,
                    "hashing {} took {} s",
                    target.display(),
                    t.elapsed().as_secs()
                );
                match result {
                    Ok(r) => say!(0, "content ids for {}: {r}", target.display()),
                    Err(e) => say!(0, "hash {}: {e:#}", target.display()),
                }
            }
        });
    }
    let running: Arc<std::sync::Mutex<HashSet<PathBuf>>> = Arc::default();
    start_schedulers(&engine, &running);
    {
        let (engine, running) = (Arc::clone(&engine), Arc::clone(&running));
        tokio::spawn(async move {
            while reload_rx.recv().await.is_some() {
                start_schedulers(&engine, &running);
            }
        });
    }
    {
        let engine = Arc::clone(&engine);
        let mut hup = signal(SignalKind::hangup())?;
        tokio::spawn(async move {
            while hup.recv().await.is_some() {
                match engine.reload().await {
                    Ok(r) => say!(0, "reloaded settings: {r}"),
                    Err(e) => say!(0, "reload failed: {e:#}"),
                }
            }
        });
    }

    let mut next_conn: u64 = 0;
    loop {
        let (stream, _) = listener.accept().await?;
        next_conn += 1;
        let pid = stream.peer_cred().ok().and_then(|c| c.pid());
        say!(2, "conn {next_conn}: opened by pid {pid:?}");
        tokio::spawn(serve(Arc::clone(&engine), stream, next_conn));
    }
}

fn bind(socket: &Path) -> Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    let dir = socket.parent().context("socket path has no parent")?;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    if std::os::unix::net::UnixStream::connect(socket).is_ok() {
        bail!(
            "another stewardd is already listening on {}",
            socket.display()
        );
    }
    let _ = std::fs::remove_file(socket);
    Ok(UnixListener::bind(socket)?)
}

async fn serve(engine: Arc<Engine>, stream: UnixStream, conn: u64) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let t = std::time::Instant::now();
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(req) => match engine.handle(req).await {
                Ok(result) => Response::Ok { result },
                Err(e) => Response::Error {
                    message: format!("{e:#}"),
                },
            },
            Err(e) => Response::Error {
                message: format!("bad request: {e}"),
            },
        };
        let mut out = serde_json::to_vec(&response).unwrap_or_default();
        let outcome = match &response {
            Response::Ok { .. } => "ok".to_string(),
            Response::Error { message } => format!("error: {message}"),
        };
        say!(
            2,
            "conn {conn}: {} -> {outcome}, {} bytes, {} ms",
            truncate(&line, 200),
            out.len(),
            t.elapsed().as_millis()
        );
        out.push(b'\n');
        if write.write_all(&out).await.is_err() {
            break;
        }
    }
    say!(2, "conn {conn}: closed");
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// One scheduler per configured root; each exits once its root is removed.
fn start_schedulers(engine: &Arc<Engine>, running: &Arc<std::sync::Mutex<HashSet<PathBuf>>>) {
    let mut set = running
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for root in engine.config().roots {
        if set.insert(root.path.clone()) {
            tokio::spawn(schedule(Arc::clone(engine), Arc::clone(running), root.path));
        }
    }
}

async fn schedule(
    engine: Arc<Engine>,
    running: Arc<std::sync::Mutex<HashSet<PathBuf>>>,
    path: PathBuf,
) {
    let mut round: u32 = 0;
    loop {
        // Re-read each round so edited intervals apply without a restart.
        let Some(root) = engine.config().roots.into_iter().find(|r| r.path == path) else {
            running
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&path);
            return;
        };
        let known = match engine.index.connect() {
            Ok(c) => engine
                .index
                .resolve(&c, &root.path)
                .await
                .ok()
                .flatten()
                .is_some(),
            Err(_) => false,
        };
        let full = !known || root.full_every == 0 || round.is_multiple_of(root.full_every);
        match engine.refresh(&root.path, !full).await {
            Ok(r) => say!(
                0,
                "{} scan of {}: {} entries, +{} ~{} -{} in {}ms",
                if full { "full" } else { "trusting" },
                r.root,
                r.entries_seen,
                r.inserted,
                r.updated,
                r.deleted,
                r.millis
            ),
            Err(e) => say!(0, "scan {}: {e:#}", root.path.display()),
        }
        round = round.wrapping_add(1);
        tokio::time::sleep(Duration::from_secs(root.interval_minutes * 60)).await;
    }
}

async fn invalidation_worker(engine: Arc<Engine>, mut rx: mpsc::UnboundedReceiver<PathBuf>) {
    while let Some(first) = rx.recv().await {
        let mut paths = vec![first];
        tokio::time::sleep(INVALIDATE_DEBOUNCE).await;
        while let Ok(p) = rx.try_recv() {
            paths.push(p);
        }
        let paths = coalesce(paths);
        say!(1, "invalidated: rescanning {paths:?}");
        for path in paths {
            // The path itself may be gone; rescan the nearest survivor.
            let Some(target) = path.ancestors().find(|a| a.is_dir()) else {
                continue;
            };
            if let Err(e) = engine.refresh(target, false).await {
                say!(0, "invalidate {}: {e:#}", target.display());
            }
        }
    }
}

/// Drop paths already covered by another path in the set.
fn coalesce(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    paths.dedup();
    let mut out: Vec<PathBuf> = Vec::new();
    for p in paths {
        if !out.last().is_some_and(|last| p.starts_with(last)) {
            out.push(p);
        }
    }
    out
}
