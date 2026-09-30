//! The steward service: owns the index, keeps it current by periodic rescans
//! and client invalidations (no inotify), and answers queries on a unix
//! socket.

mod config;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use steward_contentid::ContentId;
use steward_index::{Index, Kind, Meta, Record, ScanOptions, ScanStats};
use steward_proto::{Entry, Request, Response, ScanReport};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc};

use crate::config::Config;

/// Invalidations arriving within this window are coalesced into one rescan.
const INVALIDATE_DEBOUNCE: Duration = Duration::from_secs(2);

struct Daemon {
    index: Index,
    config: Config,
    scan_lock: Mutex<()>,
    invalidate: mpsc::UnboundedSender<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load()?;
    let index = Index::open(&config.db).await?;
    let (tx, rx) = mpsc::unbounded_channel();
    let daemon = Arc::new(Daemon {
        index,
        config,
        scan_lock: Mutex::new(()),
        invalidate: tx,
    });

    let socket = steward_proto::socket_path();
    let listener = bind(&socket)?;
    eprintln!("stewardd: listening on {}", socket.display());

    tokio::spawn(Arc::clone(&daemon).invalidation_worker(rx));
    for root in daemon.config.roots.clone() {
        tokio::spawn(Arc::clone(&daemon).schedule(root));
    }

    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(Arc::clone(&daemon).serve(stream));
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

fn report(s: &ScanStats) -> ScanReport {
    ScanReport {
        root: s.root.display().to_string(),
        dirs_read: s.dirs_read,
        dirs_trusted: s.dirs_trusted,
        entries_seen: s.entries_seen,
        inserted: s.inserted,
        updated: s.updated,
        deleted: s.deleted,
        errors: s.errors,
        millis: s.millis,
    }
}

fn kind(k: Kind) -> steward_proto::Kind {
    match k {
        Kind::File => steward_proto::Kind::File,
        Kind::Dir => steward_proto::Kind::Dir,
        Kind::Symlink => steward_proto::Kind::Symlink,
        Kind::Other => steward_proto::Kind::Other,
    }
}

fn entry(r: &Record, path: &Path, tags: Vec<String>) -> Entry {
    let m = &r.meta;
    let leaf = m.kind != Kind::Dir;
    Entry {
        path: path.display().to_string(),
        kind: kind(m.kind),
        mode: m.mode,
        uid: m.uid,
        gid: m.gid,
        size: m.size,
        alloc: m.alloc,
        mtime: m.mtime_ns / 1_000_000_000,
        total_size: if leaf { m.size } else { r.t_size },
        total_alloc: if leaf { m.alloc } else { r.t_alloc },
        total_files: r.t_files,
        total_dirs: r.t_dirs,
        tags,
        category: leaf
            .then(|| steward_classify::category(&r.name))
            .flatten()
            .map(Into::into),
    }
}

impl Daemon {
    async fn serve(self: Arc<Self>, stream: UnixStream) {
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let response = match serde_json::from_str::<Request>(&line) {
                Ok(req) => match self.handle(req).await {
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
            out.push(b'\n');
            if write.write_all(&out).await.is_err() {
                return;
            }
        }
    }

    async fn handle(&self, req: Request) -> Result<Value> {
        let conn = self.index.connect()?;
        let idx = &self.index;
        Ok(match req {
            Request::Status => {
                let mut roots = Vec::new();
                for r in idx.roots(&conn).await? {
                    roots.push(entry(&r, Path::new(&r.name), vec![]));
                }
                json!({
                    "db": self.config.db,
                    "configured": self.config.roots,
                    "indexed": roots,
                    "scanning": self.scan_lock.try_lock().is_err(),
                })
            }
            Request::Scan {
                path,
                trust_dir_mtime,
            } => {
                let opts = ScanOptions {
                    trust_dir_mtime,
                    ..ScanOptions::default()
                };
                serde_json::to_value(self.refresh(&path, opts).await?)?
            }
            Request::Invalidate { path } => {
                self.invalidate.send(path)?;
                json!("queued")
            }
            Request::Stat { path } => {
                let id = self.resolve(&conn, &path).await?;
                let r = idx.get(&conn, id).await?.context("vanished")?;
                let tags = idx.effective_tags(&conn, id).await?;
                serde_json::to_value(entry(&r, &path, tags))?
            }
            Request::Children { path } => {
                let id = self.resolve(&conn, &path).await?;
                let mut out = Vec::new();
                for r in idx.children_of(&conn, id).await? {
                    let tags = idx.tags_of(&conn, r.id).await?;
                    out.push(entry(&r, &path.join(&r.name), tags));
                }
                out.sort_by_key(|e| std::cmp::Reverse(e.total_alloc));
                serde_json::to_value(out)?
            }
            Request::Locate { pattern, limit } => {
                let hits = idx.locate(&conn, &pattern, limit).await?;
                json!(
                    hits.iter()
                        .map(|(_, p)| p.display().to_string())
                        .collect::<Vec<_>>()
                )
            }
            Request::Classify { path } => {
                let s = self.classify(&path).await?;
                json!({ "scanned": s.scanned, "tagged": s.tagged })
            }
            Request::ContentId { path } => {
                let m = Meta::lstat(&path)?;
                let cid = self.content_id(&conn, &path, &m).await?;
                json!(cid.map(|c| c.to_string()))
            }
            Request::HashTree { path } => self.hash_tree(&path).await?,
            Request::FindContent { id } => {
                let cid = ContentId::from_hex(&id, 0).context("content id is 64 hex digits")?;
                let mut cache = HashMap::new();
                let mut out = Vec::new();
                for eid in idx.find_content(&conn, &cid.root).await? {
                    out.push(
                        idx.path_of(&conn, eid, &mut cache)
                            .await?
                            .display()
                            .to_string(),
                    );
                }
                json!(out)
            }
            Request::Duplicates { path, limit } => self.duplicates(&path, limit).await?,
        })
    }

    async fn resolve(&self, conn: &turso::Connection, path: &Path) -> Result<i64> {
        self.index
            .resolve(conn, path)
            .await?
            .with_context(|| format!("{} is not indexed", path.display()))
    }

    /// Rescan, then bring the derived layers up to date for that subtree.
    async fn refresh(&self, path: &Path, opts: ScanOptions) -> Result<ScanReport> {
        let _guard = self.scan_lock.lock().await;
        let stats = self.index.scan(path, opts).await?;
        if let Err(e) = self.classify(&stats.root).await {
            eprintln!("stewardd: classify {}: {e:#}", stats.root.display());
        }
        for target in self.config.content_id_targets(&stats.root) {
            if let Err(e) = self.hash_tree(&target).await {
                eprintln!("stewardd: hash {}: {e:#}", target.display());
            }
        }
        Ok(report(&stats))
    }

    /// Classification needs a repository's ignore rules, so classify from the
    /// enclosing repository; below an already-classified directory there is
    /// nothing to do.
    async fn classify(&self, path: &Path) -> Result<steward_classify::Summary> {
        let conn = self.index.connect()?;
        let mut from = path.to_path_buf();
        for anc in path.ancestors() {
            let Some(id) = self.index.resolve(&conn, anc).await? else {
                break;
            };
            let tags = self.index.tags_of(&conn, id).await?;
            if anc != path && tags.iter().any(|t| t != "classify:repo") {
                return Ok(steward_classify::Summary::default());
            }
            if tags.iter().any(|t| t == "classify:repo") {
                from = anc.to_path_buf();
            }
        }
        steward_classify::classify(&self.index, &from).await
    }

    async fn content_id(
        &self,
        conn: &turso::Connection,
        path: &Path,
        m: &Meta,
    ) -> Result<Option<ContentId>> {
        if m.kind != Kind::File {
            bail!("{} is not a regular file", path.display());
        }
        if let Some(root) = self.index.fresh_content_id(conn, m).await? {
            return Ok(root
                .try_into()
                .ok()
                .map(|root| ContentId { root, size: m.size }));
        }
        let p = path.to_path_buf();
        let cid = tokio::task::spawn_blocking(move || steward_contentid::hash_file(&p)).await??;
        // The file changed while it was read; leave it for the next pass.
        if Meta::lstat(path)? != *m {
            return Ok(None);
        }
        if let Some(c) = cid {
            self.index.put_content_id(conn, m, &c.root).await?;
        }
        Ok(cid)
    }

    async fn hash_tree(&self, path: &Path) -> Result<Value> {
        let conn = self.index.connect()?;
        let id = self.resolve(&conn, path).await?;
        let mut stale = Vec::new();
        for (r, p) in self.index.subtree(&conn, id, path).await? {
            if r.meta.kind == Kind::File
                && r.meta.size > 0
                && self.index.fresh_content_id(&conn, &r.meta).await?.is_none()
            {
                stale.push((p, r.meta));
            }
        }
        let todo = stale.len();
        let hashed = tokio::task::spawn_blocking(move || {
            use rayon::prelude::*;
            stale
                .into_par_iter()
                .filter_map(|(p, m)| {
                    let cid = steward_contentid::hash_file(&p).ok()??;
                    (Meta::lstat(&p).ok()? == m).then_some((m, cid))
                })
                .collect::<Vec<_>>()
        })
        .await?;
        conn.execute("BEGIN", ()).await?;
        for (m, cid) in &hashed {
            self.index.put_content_id(&conn, m, &cid.root).await?;
        }
        conn.execute("COMMIT", ()).await?;
        Ok(json!({ "stale": todo, "hashed": hashed.len() }))
    }

    async fn duplicates(&self, path: &Path, limit: u32) -> Result<Value> {
        let conn = self.index.connect()?;
        let id = self.resolve(&conn, path).await?;
        let mut groups: BTreeMap<Vec<u8>, (u64, Vec<String>)> = BTreeMap::new();
        for (r, p) in self.index.subtree(&conn, id, path).await? {
            if r.meta.kind != Kind::File {
                continue;
            }
            if let Some(root) = self.index.fresh_content_id(&conn, &r.meta).await? {
                let g = groups.entry(root).or_insert((r.meta.size, vec![]));
                g.1.push(p.display().to_string());
            }
        }
        let mut dups: Vec<_> = groups
            .into_iter()
            .filter(|(_, (_, paths))| paths.len() > 1)
            .map(|(root, (size, paths))| {
                let wasted = size * (paths.len() as u64 - 1);
                (wasted, steward_contentid::hex(&root), size, paths)
            })
            .collect();
        dups.sort_by_key(|d| std::cmp::Reverse(d.0));
        dups.truncate(limit as usize);
        Ok(json!(
            dups.into_iter()
                .map(|(wasted, id, size, paths)| json!({
                    "id": format!("btv2:{id}"), "size": size, "wasted": wasted, "paths": paths,
                }))
                .collect::<Vec<_>>()
        ))
    }

    async fn schedule(self: Arc<Self>, root: config::Root) {
        let mut round: u32 = 0;
        loop {
            let conn = self.index.connect();
            let known = match &conn {
                Ok(c) => self
                    .index
                    .resolve(c, &root.path)
                    .await
                    .ok()
                    .flatten()
                    .is_some(),
                Err(_) => false,
            };
            let full = !known || root.full_every == 0 || round.is_multiple_of(root.full_every);
            let opts = ScanOptions {
                trust_dir_mtime: !full,
                one_filesystem: root.one_filesystem,
            };
            match self.refresh(&root.path, opts).await {
                Ok(r) => eprintln!(
                    "stewardd: {} scan of {}: {} entries, +{} ~{} -{} in {}ms",
                    if full { "full" } else { "trusting" },
                    r.root,
                    r.entries_seen,
                    r.inserted,
                    r.updated,
                    r.deleted,
                    r.millis
                ),
                Err(e) => eprintln!("stewardd: scan {}: {e:#}", root.path.display()),
            }
            round = round.wrapping_add(1);
            tokio::time::sleep(Duration::from_secs(root.interval_minutes * 60)).await;
        }
    }

    async fn invalidation_worker(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<PathBuf>) {
        while let Some(first) = rx.recv().await {
            let mut paths = vec![first];
            tokio::time::sleep(INVALIDATE_DEBOUNCE).await;
            while let Ok(p) = rx.try_recv() {
                paths.push(p);
            }
            for path in coalesce(paths) {
                // The path itself may be gone; rescan the nearest survivor.
                let Some(target) = path.ancestors().find(|a| a.is_dir()) else {
                    continue;
                };
                if let Err(e) = self.refresh(target, ScanOptions::default()).await {
                    eprintln!("stewardd: invalidate {}: {e:#}", target.display());
                }
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
