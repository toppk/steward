//! The daemon: owns the index, keeps it current by periodic rescans
//! and client invalidations (no inotify), and answers JSON-RPC 2.0 on two Unix
//! sockets: `api.socket` for administration and `content.socket` for the
//! application-facing content primitives.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::engine::Engine;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use steward_proto::{JSONRPC, Request, RpcError, RpcRequest, RpcResponse, code};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tracing::Instrument;

/// Invalidations arriving within this window are coalesced into one rescan.
const INVALIDATE_DEBOUNCE: Duration = Duration::from_secs(2);

/// Run the daemon until it fails: load settings, open the index, start
/// the schedulers and hashing worker, and serve both sockets.
pub async fn run() -> Result<()> {
    let (tx, rx) = mpsc::unbounded_channel();
    let (reload_tx, mut reload_rx) = mpsc::unbounded_channel();
    let (hash_tx, mut hash_rx) = mpsc::unbounded_channel::<PathBuf>();
    let config = Config::load()?;
    tracing::debug!(
        "settings {}, index {}",
        Config::path().display(),
        config.db.display()
    );
    for r in &config.roots {
        crate::engine::describe_root(r);
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
    let content_socket = steward_proto::content_socket_path();
    let content_listener = bind(&content_socket)?;
    tracing::info!(
        "listening on {} (administration) and {} (content)",
        socket.display(),
        content_socket.display()
    );

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
                tracing::debug!(
                    "hashing {} took {} s",
                    target.display(),
                    t.elapsed().as_secs()
                );
                match result {
                    Ok(r) => tracing::info!("content ids for {}: {r}", target.display()),
                    Err(e) => tracing::error!("hash {}: {e:#}", target.display()),
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
        let mut usr2 = signal(SignalKind::user_defined2())?;
        tokio::spawn(async move {
            while usr2.recv().await.is_some() {
                let report =
                    serde_json::to_string_pretty(&engine.activity_report()).unwrap_or_default();
                tracing::info!("activity: {report}");
            }
        });
    }
    {
        let engine = Arc::clone(&engine);
        let mut hup = signal(SignalKind::hangup())?;
        tokio::spawn(async move {
            while hup.recv().await.is_some() {
                match engine.reload().await {
                    Ok(r) => tracing::info!("reloaded settings: {r}"),
                    Err(e) => tracing::error!("reload failed: {e:#}"),
                }
            }
        });
    }

    let mut next_conn: u64 = 0;
    loop {
        let (stream, admin) = tokio::select! {
            r = listener.accept() => (r?.0, true),
            r = content_listener.accept() => (r?.0, false),
        };
        next_conn += 1;
        let pid = stream.peer_cred().ok().and_then(|c| c.pid());
        let which = if admin { "api" } else { "content" };
        let span = tracing::debug_span!("conn", id = next_conn, socket = %which);
        span.in_scope(|| tracing::trace!("opened by pid {pid:?}"));
        tokio::spawn(serve(Arc::clone(&engine), stream, admin).instrument(span));
    }
}

/// What `content.socket` offers: reads of the catalog and the content
/// primitives. Configuration, maintenance jobs and file exports are
/// administration and stay on `api.socket`.
fn application_method(req: &Request) -> bool {
    matches!(
        req,
        Request::Status
            | Request::Stat { .. }
            | Request::Children { .. }
            | Request::Locate { .. }
            | Request::ContentId { .. }
            | Request::FindContent { .. }
            | Request::Duplicates { .. }
            | Request::ContentSummary { .. }
            | Request::PieceLayer { .. }
            | Request::Resolve { .. }
            | Request::Inspect { .. }
            | Request::Verify { .. }
            | Request::Invalidate { .. }
            | Request::Subscribe { .. }
            | Request::Unsubscribe
    )
}

fn line(msg: &impl serde::Serialize) -> Vec<u8> {
    let mut out = serde_json::to_vec(msg).unwrap_or_default();
    out.push(b'\n');
    out
}

fn reply(id: Value, result: Result<Value, RpcError>) -> Vec<u8> {
    let (result, error) = match result {
        Ok(r) => (Some(r), None),
        Err(e) => (None, Some(e)),
    };
    line(&RpcResponse {
        jsonrpc: JSONRPC.into(),
        id,
        result,
        error,
    })
}

fn notification(method: &str, params: impl serde::Serialize) -> Vec<u8> {
    line(&json!({ "jsonrpc": JSONRPC, "method": method, "params": params }))
}

/// Starts delivering events on a connection; returns the subscribe result
/// and the task forwarding them.
fn subscribe(
    engine: &Arc<Engine>,
    since: Option<u64>,
    ids: Option<Vec<String>>,
    out: mpsc::Sender<Vec<u8>>,
) -> Result<(Value, tokio::task::JoinHandle<()>), RpcError> {
    let ids = match ids {
        Some(ids) => Some(
            ids.iter()
                .map(|i| crate::engine::normalize_id(i))
                .collect::<Result<HashSet<_>>>()
                .map_err(|e| {
                    RpcError::new(code::INVALID_PARAMS, "invalid_params", e.to_string())
                })?,
        ),
        None => None,
    };
    let start = engine.events.subscribe(since);
    let result = json!({
        "epoch": engine.events.epoch,
        "seq": start.seq,
        "complete": start.complete,
    });
    let mut live = start.live;
    let replay = start.replay;
    let counter = Arc::clone(engine);
    let task = tokio::spawn(
        async move {
            use tokio::sync::broadcast::error::RecvError;
            let _counted = crate::activity::Counted::new(&counter.activity.subscribers);
            let mut last = 0;
            for r in replay {
                last = r.seq;
                if crate::events::wanted(ids.as_ref(), &r)
                    && out.send(notification("event", &*r)).await.is_err()
                {
                    return;
                }
            }
            loop {
                match live.recv().await {
                    Ok(r) => {
                        if r.seq <= last {
                            continue;
                        }
                        last = r.seq;
                        if crate::events::wanted(ids.as_ref(), &r)
                            && out.send(notification("event", &*r)).await.is_err()
                        {
                            return;
                        }
                    }
                    Err(RecvError::Lagged(_)) => {
                        if out
                            .send(notification("gap", json!({ "after": last })))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(RecvError::Closed) => return,
                }
            }
        }
        .instrument(tracing::Span::current()),
    );
    Ok((result, task))
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
    // Only ever remove a stale socket of ours, never anything else.
    if let Ok(m) = std::fs::symlink_metadata(socket) {
        use std::os::unix::fs::FileTypeExt;
        if !m.file_type().is_socket() {
            bail!(
                "{} exists and is not a socket; not touching it",
                socket.display()
            );
        }
        std::fs::remove_file(socket)?;
    }
    Ok(UnixListener::bind(socket)?)
}

async fn serve(engine: Arc<Engine>, stream: UnixStream, admin: bool) {
    let _counted = crate::activity::Counted::new(&engine.activity.connections);
    let (read, mut write) = stream.into_split();
    let (out, mut outgoing) = mpsc::channel::<Vec<u8>>(1024);
    let writer = tokio::spawn(async move {
        while let Some(bytes) = outgoing.recv().await {
            if write.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });
    let mut events: Option<tokio::task::JoinHandle<()>> = None;
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(text)) = lines.next_line().await {
        let msg: RpcRequest = match serde_json::from_str::<Value>(&text) {
            Err(e) => {
                let e = RpcError::new(code::PARSE, "parse_error", e.to_string());
                let _ = out.send(reply(Value::Null, Err(e))).await;
                continue;
            }
            Ok(v) => {
                let id = v.get("id").cloned().unwrap_or(Value::Null);
                match serde_json::from_value::<RpcRequest>(v) {
                    Ok(m) if m.jsonrpc == JSONRPC => m,
                    _ => {
                        let e = RpcError::new(
                            code::INVALID_REQUEST,
                            "invalid_request",
                            "not a JSON-RPC 2.0 request",
                        );
                        let _ = out.send(reply(id, Err(e))).await;
                        continue;
                    }
                }
            }
        };
        let id = msg.id.clone();
        let req = msg.request().and_then(|req| {
            if admin || application_method(&req) {
                Ok(req)
            } else {
                Err(RpcError::new(
                    code::FAILED,
                    "forbidden",
                    format!(
                        "{} is administration: use {}",
                        msg.method,
                        steward_proto::socket_path().display()
                    ),
                ))
            }
        });
        let result = match req {
            Err(e) => Err(e),
            Ok(Request::Subscribe { since, ids }) => {
                if let Some(t) = events.take() {
                    t.abort();
                }
                // The result goes out before the first replayed event.
                match subscribe(&engine, since, ids, out.clone()) {
                    Ok((result, task)) => {
                        if let Some(id) = &id {
                            let _ = out.send(reply(id.clone(), Ok(result))).await;
                        }
                        events = Some(task);
                        tracing::trace!("subscribed");
                        continue;
                    }
                    Err(e) => Err(e),
                }
            }
            Ok(Request::Unsubscribe) => {
                if let Some(t) = events.take() {
                    t.abort();
                }
                Ok(Value::Bool(true))
            }
            Ok(req) => {
                // Requests run concurrently; responses carry their ids.
                let (engine, out) = (Arc::clone(&engine), out.clone());
                tokio::spawn(
                    async move {
                        let t = std::time::Instant::now();
                        let result = engine.handle(req).await.map_err(|e| {
                            RpcError::new(
                                code::FAILED,
                                crate::engine::kind_of(&e),
                                format!("{e:#}"),
                            )
                        });
                        let outcome = match &result {
                            Ok(_) => "ok".to_string(),
                            Err(e) => format!("{}: {}", e.kind(), e.message),
                        };
                        tracing::trace!(
                            "{} -> {outcome}, {} ms",
                            truncate(&text, 200),
                            t.elapsed().as_millis()
                        );
                        if let Some(id) = id {
                            let _ = out.send(reply(id, result)).await;
                        }
                    }
                    .instrument(tracing::Span::current()),
                );
                continue;
            }
        };
        if let Some(id) = id {
            let _ = out.send(reply(id, result)).await;
        }
    }
    if let Some(t) = events.take() {
        t.abort();
    }
    drop(out);
    let _ = writer.await;
    tracing::trace!("closed");
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
            engine
                .next_scans
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
            Ok(r) => tracing::info!(
                "{} scan of {}: {} entries, +{} ~{} -{} in {}ms",
                if full { "full" } else { "trusting" },
                r.root,
                r.entries_seen,
                r.inserted,
                r.updated,
                r.deleted,
                r.millis
            ),
            Err(e) => tracing::error!("scan {}: {e:#}", root.path.display()),
        }
        round = round.wrapping_add(1);
        let wait = Duration::from_secs(root.interval_minutes * 60);
        let next = std::time::SystemTime::now() + wait;
        let next = next
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64());
        engine
            .next_scans
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root.path.clone(), next);
        tokio::time::sleep(wait).await;
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
        tracing::debug!("invalidated: rescanning {paths:?}");
        for path in paths {
            // The path itself may be gone; rescan the nearest survivor.
            let Some(target) = path.ancestors().find(|a| a.is_dir()) else {
                continue;
            };
            if let Err(e) = engine.refresh(target, false).await {
                tracing::error!("invalidate {}: {e:#}", target.display());
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
