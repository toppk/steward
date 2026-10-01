//! Request handling over an index, shared by the daemon and the CLI's
//! offline `--db` mode.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use steward_contentid::ContentId;
use steward_index::{ChangeKind, Index, Kind, Meta, Record, ScanOptions, ScanStats};
use steward_proto::{ContentRef, Entry, Request, ScanReport};
use tokio::sync::{Mutex, mpsc};

use crate::config::Config;

/// An error with a stable `data.type` for clients; any other error is
/// reported as `failed`.
#[derive(Debug)]
pub struct Failure {
    pub kind: &'static str,
    pub message: String,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failure {}

pub fn fail(kind: &'static str, message: impl Into<String>) -> anyhow::Error {
    Failure {
        kind,
        message: message.into(),
    }
    .into()
}

/// The stable type of an error from `handle`.
pub fn kind_of(e: &anyhow::Error) -> &'static str {
    e.downcast_ref::<Failure>().map_or("failed", |f| f.kind)
}

fn content_hex(root: &[u8]) -> String {
    format!("btv2:{}", steward_contentid::hex(root))
}

/// A content id from a client, with or without its `btv2:` prefix.
pub fn parse_id(id: &str) -> Result<[u8; 32]> {
    ContentId::from_hex(&id.to_ascii_lowercase(), 0)
        .map(|c| c.root)
        .ok_or_else(|| {
            fail(
                "invalid_params",
                format!("{id:?} is not a content id (btv2:<64 hex>)"),
            )
        })
}

/// Canonical form of a client's content id, for matching event filters.
pub fn normalize_id(id: &str) -> Result<String> {
    parse_id(id).map(|r| content_hex(&r))
}

/// Marks a path as under verification until dropped.
struct Suspect<'a>(&'a std::sync::Mutex<HashSet<PathBuf>>, PathBuf);

impl Drop for Suspect<'_> {
    fn drop(&mut self) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.1);
    }
}

/// How hashing one file went.
enum Hashing {
    Done(Option<steward_contentid::Hashed>),
    Changing,
    Failed(String),
}

/// A hashing job in flight, reported by `status`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct HashProgress {
    pub path: std::path::PathBuf,
    pub files_total: u64,
    pub files_done: u64,
    pub bytes_total: u64,
    pub bytes_done: u64,
}

fn build_hash_pool(threads: usize) -> std::sync::Arc<rayon::ThreadPool> {
    std::sync::Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("steward-hash-{i}"))
            .build()
            .expect("hash thread pool"),
    )
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / f64::from(1u32 << 30))
}

fn duration(secs: f64) -> String {
    let s = secs as u64;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        _ => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}

pub struct Engine {
    pub events: crate::events::Bus,
    /// Paths under `verify`: withheld from `resolve` until it answers.
    suspect: std::sync::Mutex<HashSet<PathBuf>>,
    hashing: std::sync::Mutex<Option<HashProgress>>,
    /// Directories the last scans found on an unmounted volume.
    offline: std::sync::Mutex<Vec<std::path::PathBuf>>,
    /// A pool of its own, so a days-long hashing job never takes the threads
    /// directory scans need; replaced when `hash_threads` changes.
    hash_pool: std::sync::RwLock<std::sync::Arc<rayon::ThreadPool>>,
    pub index: Index,
    config: std::sync::RwLock<Config>,
    pub scan_lock: Mutex<()>,
    /// Where `invalidate` requests go; without one they rescan immediately.
    pub invalidate: Option<mpsc::UnboundedSender<std::path::PathBuf>>,
    /// Where content-id work goes; hashing can take hours on media, so the
    /// daemon runs it outside the scan lock. Without one it runs inline.
    pub hash_queue: Option<mpsc::UnboundedSender<std::path::PathBuf>>,
    /// Folders waiting in `hash_queue`, so rescans during a long job queue
    /// one follow-up at most instead of one per rescan.
    hash_waiting: std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>,
    /// Told after a reload so the daemon can start schedulers for new roots.
    pub reloaded: Option<mpsc::UnboundedSender<()>>,
    /// Roots come only from the config: scans outside them are refused and
    /// removed roots are pruned. Off for the CLI's ad-hoc `--db` use.
    pub managed: bool,
}

/// One `-v` line per root: where it is and what runs on it.
pub fn describe_root(r: &crate::config::Root) {
    crate::say!(
        1,
        "root {}: rescan every {} min, full every {}, one_filesystem={}, classify={}, \
         exclude={:?}, contentid={:?}",
        r.path.display(),
        r.interval_minutes,
        r.full_every,
        r.one_filesystem,
        r.classify,
        r.exclude,
        r.contentid
    );
}

/// Normalise and check a root from a client before it reaches the file.
fn validate_root(mut r: crate::config::Root) -> Result<crate::config::Root> {
    r.path = crate::config::expand(&r.path);
    if !r.path.is_absolute() {
        bail!("root path must be absolute: {}", r.path.display());
    }
    if !r.path.is_dir() {
        bail!("{} is not a directory", r.path.display());
    }
    if r.interval_minutes == 0 {
        bail!("interval_minutes must be at least 1");
    }
    let mut b = ignore::gitignore::GitignoreBuilder::new(&r.path);
    for pat in &r.exclude {
        b.add_line(None, pat)
            .with_context(|| format!("exclude pattern {pat:?}"))?;
    }
    b.build()?;
    r.exclude.retain(|p| !p.trim().is_empty());
    for c in &mut r.contentid {
        *c = r.path.join(crate::config::expand(c));
        if !c.starts_with(&r.path) {
            bail!("content id folder {} is outside the root", c.display());
        }
    }
    Ok(r)
}

pub fn report(s: &ScanStats) -> ScanReport {
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
        offline: s.offline.iter().map(|p| p.display().to_string()).collect(),
        load_ms: s.load_ms,
        write_ms: s.write_ms,
        totals_ms: s.totals_ms,
    }
}

pub fn kind(k: Kind) -> steward_proto::Kind {
    match k {
        Kind::File => steward_proto::Kind::File,
        Kind::Dir => steward_proto::Kind::Dir,
        Kind::Symlink => steward_proto::Kind::Symlink,
        Kind::Other | Kind::BlockDev | Kind::CharDev | Kind::Fifo | Kind::Socket => {
            steward_proto::Kind::Other
        }
    }
}

pub fn entry(r: &Record, path: &Path, tags: Vec<String>) -> Entry {
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
        total_files: if leaf {
            u64::from(m.kind == Kind::File)
        } else {
            r.t_files
        },
        total_dirs: if leaf { 0 } else { r.t_dirs },
        tags,
        category: leaf
            .then(|| steward_classify::category(&r.name))
            .flatten()
            .map(Into::into),
        content_id: None,
    }
}

impl Engine {
    pub async fn open(config: Config) -> Result<Self> {
        let index = Index::open(&config.db).await?;
        crate::say!(1, "hashing uses {} threads", config.hash_threads());
        Ok(Self {
            events: crate::events::Bus::default(),
            suspect: std::sync::Mutex::default(),
            hashing: std::sync::Mutex::new(None),
            offline: std::sync::Mutex::default(),
            hash_pool: std::sync::RwLock::new(build_hash_pool(config.hash_threads())),
            index,
            config: std::sync::RwLock::new(config),
            scan_lock: Mutex::new(()),
            invalidate: None,
            hash_queue: None,
            hash_waiting: std::sync::Mutex::default(),
            reloaded: None,
            managed: false,
        })
    }

    pub fn config(&self) -> Config {
        self.config
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Scan settings for `path` from the root it belongs to.
    pub fn scan_options(&self, path: &Path, trust_dir_mtime: bool) -> ScanOptions {
        let config = self.config();
        let root = config.root_for(path);
        ScanOptions {
            trust_dir_mtime,
            one_filesystem: root.is_none_or(|r| r.one_filesystem),
            exclude: root.map(|r| r.exclude.clone()).unwrap_or_default(),
            exclude_base: root.map(|r| r.path.clone()),
            progress: crate::log::enabled(2).then(|| {
                let path = path.display().to_string();
                steward_index::Progress(std::sync::Arc::new(move |line| {
                    crate::say!(2, "scan {path}: {line}");
                }))
            }),
            ..ScanOptions::default()
        }
    }

    /// Drop indexed roots no configured root covers.
    pub async fn prune(&self) -> Result<Vec<String>> {
        let config = self.config();
        let conn = self.index.connect()?;
        let mut removed = Vec::new();
        for r in self.index.roots(&conn).await? {
            let path = std::path::PathBuf::from(&r.name);
            if !config.covers(&path) {
                let _guard = self.scan_lock.lock().await;
                let n = self.index.remove_root(&path).await?;
                crate::say!(
                    0,
                    "removed {} ({n} entries): no longer configured",
                    path.display()
                );
                self.events
                    .emit("storage.unindexed", json!({ "path": path }));
                removed.push(path.display().to_string());
            }
        }
        Ok(removed)
    }

    /// Re-read settings; prune removed roots and rescan changed ones.
    pub async fn reload(&self) -> Result<Value> {
        let new = Config::load()?;
        let old = std::mem::replace(
            &mut *self
                .config
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            new.clone(),
        );
        let added: Vec<_> = new
            .roots
            .iter()
            .filter(|r| !old.roots.iter().any(|o| o.path == r.path))
            .collect();
        let changed: Vec<_> = new
            .roots
            .iter()
            .filter(|r| old.roots.iter().any(|o| o.path == r.path && o != *r))
            .collect();
        if new.hash_threads() != old.hash_threads() {
            // Running jobs keep their pool; new ones use this one.
            *self
                .hash_pool
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                build_hash_pool(new.hash_threads());
            crate::say!(1, "hashing now uses {} threads", new.hash_threads());
        }
        let removed = if self.managed {
            self.prune().await?
        } else {
            Vec::new()
        };
        if let Some(tx) = &self.invalidate {
            for r in &changed {
                let _ = tx.send(r.path.clone());
            }
        }
        if let Some(tx) = &self.reloaded {
            let _ = tx.send(());
        }
        crate::say!(
            1,
            "reloaded {}: added {:?}, changed {:?}, removed {:?}",
            Config::path().display(),
            added.iter().map(|r| &r.path).collect::<Vec<_>>(),
            changed.iter().map(|r| &r.path).collect::<Vec<_>>(),
            removed
        );
        for r in added.iter().chain(&changed) {
            describe_root(r);
        }
        Ok(json!({
            "added": added.iter().map(|r| &r.path).collect::<Vec<_>>(),
            "changed": changed.iter().map(|r| &r.path).collect::<Vec<_>>(),
            "removed": removed,
        }))
    }

    pub async fn handle(&self, req: Request) -> Result<Value> {
        let conn = self.index.connect()?;
        let idx = &self.index;
        Ok(match req {
            Request::Status => {
                let mut roots = Vec::new();
                for r in idx.roots(&conn).await? {
                    roots.push(entry(&r, Path::new(&r.name), vec![]));
                }
                json!({
                    "db": self.config().db,
                    "configured": self.config().roots,
                    "indexed": roots,
                    "scanning": self.scan_lock.try_lock().is_err(),
                    "hashing": self.hash_progress(),
                })
            }
            Request::Scan {
                path,
                trust_dir_mtime,
            } => {
                self.require_root(&path)?;
                serde_json::to_value(self.refresh(&path, trust_dir_mtime).await?)?
            }
            Request::Invalidate { path } => match &self.invalidate {
                Some(tx) => {
                    tx.send(path)?;
                    json!("queued")
                }
                None => serde_json::to_value(self.refresh(&path, false).await?)?,
            },
            Request::ExportQdirstat { path, out } => {
                let id = self.resolve(&conn, &path).await?;
                let n = crate::qdirstat::export(idx, &conn, id, &path, &out).await?;
                json!({ "entries": n, "out": out })
            }
            Request::Stat { path } => {
                let id = self.resolve(&conn, &path).await?;
                let r = idx.get(&conn, id).await?.context("vanished")?;
                let tags = idx.effective_tags(&conn, id).await?;
                let mut e = entry(&r, &path, tags);
                e.content_id = self.stored_id(&conn, &r).await?;
                serde_json::to_value(e)?
            }
            Request::Children { path } => {
                let id = self.resolve(&conn, &path).await?;
                let mut out = Vec::new();
                for r in idx.children_of(&conn, id).await? {
                    let tags = idx.tags_of(&conn, r.id).await?;
                    let mut e = entry(&r, &path.join(&r.name), tags);
                    e.content_id = self.stored_id(&conn, &r).await?;
                    out.push(e);
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
                let root = parse_id(&id)?;
                let mut cache = HashMap::new();
                let mut out = Vec::new();
                for eid in idx.find_content(&conn, &root).await? {
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
            Request::Reload => self.reload().await?,
            Request::Settings => self.settings().await?,
            Request::PutRoot { root } => {
                let root = validate_root(root)?;
                crate::settings_file::put_root(&Config::path(), &root)?;
                crate::say!(1, "settings: put root {}", root.path.display());
                self.reload().await?
            }
            Request::RemoveRoot { path } => {
                let path = crate::config::expand(&path);
                if !crate::settings_file::remove_root(&Config::path(), &path)? {
                    bail!("{} is not a configured root", path.display());
                }
                crate::say!(1, "settings: removed root {}", path.display());
                self.reload().await?
            }
            Request::ContentSummary { path } => self.content_summary(&path).await?,
            Request::PieceLayer { id, piece_size } => self.piece_layer(&id, piece_size).await?,
            Request::Resolve { contents, recheck } => {
                self.resolve_contents(contents, recheck).await?
            }
            Request::Inspect { paths } => self.inspect(paths).await?,
            Request::Verify { id, path, reason } => self.verify(&id, &path, &reason).await?,
            Request::Subscribe { .. } | Request::Unsubscribe => {
                bail!("subscriptions belong to a connection; send them to the daemon")
            }
        })
    }

    fn require_root(&self, path: &Path) -> Result<()> {
        if self.managed && !self.config().covers(path) {
            return Err(fail(
                "not_under_root",
                format!(
                    "{} is not under a configured root; add a [[root]] to {} and run \
                     `steward reload`",
                    path.display(),
                    Config::path().display()
                ),
            ));
        }
        Ok(())
    }

    /// Current observations of each content, in request order.
    async fn resolve_contents(&self, contents: Vec<ContentRef>, recheck: bool) -> Result<Value> {
        let roots = contents
            .iter()
            .map(|c| parse_id(&c.id))
            .collect::<Result<Vec<_>>>()?;
        let conn = self.index.connect()?;
        if recheck {
            steward_index::scan::forget_filesystems();
        }
        let offline = self
            .offline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let suspect = self
            .suspect
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut cache = HashMap::new();
        let mut stale: HashSet<PathBuf> = HashSet::new();
        let mut out = Vec::new();
        for (c, root) in contents.iter().zip(&roots) {
            let id = content_hex(root);
            let known = self.index.known_size(&conn, root).await?;
            if let (Some(want), Some(have)) = (c.size, known)
                && want != have
            {
                out.push(json!({
                    "id": id, "size": have, "state": "mismatch", "layer": false,
                    "observations": [],
                }));
                continue;
            }
            let mut obs = Vec::new();
            for eid in self.index.find_content(&conn, root).await? {
                let Some(rec) = self.index.get(&conn, eid).await? else {
                    continue;
                };
                let path = self.index.path_of(&conn, eid, &mut cache).await?;
                if suspect.contains(&path) {
                    continue;
                }
                let mut offline_at = offline.iter().find(|o| path.starts_with(o)).cloned();
                if recheck {
                    match Meta::lstat(&path) {
                        Ok(m)
                            if m.dev == rec.meta.dev
                                && m.ino == rec.meta.ino
                                && m.size == rec.meta.size
                                && m.mtime_ns == rec.meta.mtime_ns =>
                        {
                            // Reachable after all: its volume is back.
                            if let Some(dir) = offline_at.take() {
                                stale.insert(dir);
                            }
                        }
                        // A path on another filesystem: maybe a different
                        // volume mounted where this one was.
                        Ok(m) if m.dev == rec.meta.dev => {
                            stale.insert(path.parent().unwrap_or(&path).to_path_buf());
                            continue;
                        }
                        Ok(_) | Err(_) => match self.offline_ancestor(&conn, &path).await? {
                            Some(dir) => offline_at = Some(dir),
                            None => {
                                stale.insert(path.parent().unwrap_or(&path).to_path_buf());
                                continue;
                            }
                        },
                    }
                }
                obs.push((
                    offline_at.is_none(),
                    json!({
                        "path": path,
                        "inode": format!("{:x}:{}", rec.meta.dev, rec.meta.ino),
                        "online": offline_at.is_none(),
                        "offline_at": offline_at,
                        "mtime_ns": rec.meta.mtime_ns,
                    }),
                ));
            }
            obs.sort_by(|a, b| {
                b.0.cmp(&a.0)
                    .then_with(|| a.1["path"].as_str().cmp(&b.1["path"].as_str()))
            });
            let state = if obs.iter().any(|o| o.0) {
                "present"
            } else if !obs.is_empty() {
                "offline"
            } else if known.is_some() {
                "absent"
            } else {
                "unknown"
            };
            let layer = match known {
                Some(size) => self.has_layer(&conn, root, size).await?,
                None => false,
            };
            out.push(json!({
                "id": id,
                "size": known,
                "state": state,
                "layer": layer,
                "observations": obs.into_iter().map(|o| o.1).collect::<Vec<_>>(),
            }));
        }
        // Copies found changed or gone: let a rescan bring the catalog along.
        if let Some(tx) = &self.invalidate {
            for dir in stale {
                let _ = tx.send(dir);
            }
        }
        Ok(json!(out))
    }

    /// The indexed directory above `path` whose volume is not mounted now:
    /// the topmost of the existing ancestors found on another filesystem
    /// than the index recorded, the same directory a scan reports offline.
    async fn offline_ancestor(
        &self,
        conn: &turso::Connection,
        path: &Path,
    ) -> Result<Option<PathBuf>> {
        let mut found = None;
        for dir in path.ancestors().skip(1) {
            let Ok(m) = Meta::lstat(dir) else {
                if found.is_some() {
                    break;
                }
                continue;
            };
            let moved = match self.index.resolve(conn, dir).await? {
                Some(id) => self
                    .index
                    .get(conn, id)
                    .await?
                    .is_some_and(|r| r.meta.kind == Kind::Dir && r.meta.dev != m.dev),
                None => false,
            };
            if !moved {
                break;
            }
            found = Some(dir.to_path_buf());
        }
        Ok(found)
    }

    /// Bring the catalog up to date for `paths` now and establish the
    /// content ids of the regular files among them.
    async fn inspect(&self, paths: Vec<PathBuf>) -> Result<Value> {
        let item_error = |path: &Path, e: &anyhow::Error| {
            json!({
                "path": path, "kind": null, "id": null, "size": 0,
                "error": { "type": kind_of(e), "message": format!("{e:#}") },
            })
        };
        let mut out: Vec<Option<Value>> = vec![None; paths.len()];
        let mut dirs = Vec::new();
        let mut parents = std::collections::BTreeSet::new();
        for (i, path) in paths.iter().enumerate() {
            let checked = if path.is_absolute() {
                self.require_root(path)
            } else {
                Err(fail(
                    "invalid_params",
                    format!("{} is not absolute", path.display()),
                ))
            };
            if let Err(e) = checked {
                out[i] = Some(item_error(path, &e));
                continue;
            }
            match Meta::lstat(path) {
                Ok(m) if m.kind == Kind::Dir => dirs.push(i),
                _ => {
                    parents.insert(path.parent().unwrap_or(path).to_path_buf());
                }
            }
        }
        for &i in &dirs {
            if let Err(e) = self.refresh(&paths[i], false).await {
                out[i] = Some(item_error(&paths[i], &e));
            }
        }
        for dir in parents {
            let target = dir.ancestors().find(|a| a.is_dir()).unwrap_or(&dir);
            if let Err(e) = self.refresh_with(target, true, true).await {
                crate::say!(0, "inspect: scan of {}: {e:#}", target.display());
            }
        }

        let conn = self.index.connect()?;
        let mut to_hash = Vec::new();
        for (i, path) in paths.iter().enumerate() {
            if out[i].is_some() {
                continue;
            }
            let m = match Meta::lstat(path) {
                Ok(m) => m,
                Err(e) => {
                    let kind = if e.kind() == std::io::ErrorKind::NotFound {
                        "not_found"
                    } else {
                        "unreadable"
                    };
                    out[i] = Some(item_error(
                        path,
                        &fail(kind, format!("{}: {e}", path.display())),
                    ));
                    continue;
                }
            };
            let kind = serde_json::to_value(kind(m.kind))?;
            if m.kind != Kind::File || m.size == 0 {
                out[i] = Some(json!({
                    "path": path, "kind": kind, "id": null, "size": m.size, "error": null,
                }));
                continue;
            }
            if let Some(root) = self.index.fresh_content_id(&conn, &m).await?
                && self.has_layer(&conn, &root, m.size).await?
            {
                out[i] = Some(json!({
                    "path": path, "kind": kind, "id": content_hex(&root), "size": m.size,
                    "error": null,
                }));
                continue;
            }
            to_hash.push((i, path.clone(), m));
        }
        let files = to_hash.iter().map(|(_, p, m)| (p.clone(), *m)).collect();
        let results = self.hash_now(files).await?;
        for ((i, path, m), result) in to_hash.into_iter().zip(results) {
            let kind = serde_json::to_value(kind(m.kind))?;
            out[i] = Some(match result {
                Hashing::Done(h) => {
                    let id = match h {
                        Some(h) => {
                            self.record_hash(&conn, &path, &m, &h).await?;
                            Some(content_hex(&h.id.root))
                        }
                        None => None,
                    };
                    json!({ "path": path, "kind": kind, "id": id, "size": m.size, "error": null })
                }
                Hashing::Changing => item_error(
                    &path,
                    &fail(
                        "changing",
                        format!("{} changed while being read", path.display()),
                    ),
                ),
                Hashing::Failed(e) => item_error(&path, &fail("unreadable", e)),
            });
        }
        Ok(json!(
            out.into_iter()
                .map(Option::unwrap_or_default)
                .collect::<Vec<_>>()
        ))
    }

    /// Reread `path` in full and record what it holds, whatever the stored
    /// stat says; `id` is what the caller believed it held.
    async fn verify(&self, id: &str, path: &Path, reason: &str) -> Result<Value> {
        let claimed = parse_id(id)?;
        if !path.is_absolute() {
            return Err(fail(
                "invalid_params",
                format!("{} is not absolute", path.display()),
            ));
        }
        self.require_root(path)?;
        crate::say!(
            1,
            "verify {} as {}: {reason}",
            path.display(),
            content_hex(&claimed)
        );
        self.suspect
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(path.to_path_buf());
        let _suspect = Suspect(&self.suspect, path.to_path_buf());
        let answer = |state: &str, current: Option<String>| {
            Ok(json!({
                "id": content_hex(&claimed), "path": path, "state": state, "current": current,
            }))
        };
        let parent = path.parent().unwrap_or(path);
        if let Some(target) = parent.ancestors().find(|a| a.is_dir()) {
            self.refresh_with(target, true, true).await?;
        }
        let m = match Meta::lstat(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return answer("gone", None),
            Err(_) => return answer("unreadable", None),
        };
        if m.kind != Kind::File {
            return answer("not_file", None);
        }
        let conn = self.index.connect()?;
        let before = self.index.fresh_content_id(&conn, &m).await?;
        let mut result = Hashing::Changing;
        for _ in 0..3 {
            let m = Meta::lstat(path)?;
            result = self
                .hash_now(vec![(path.to_path_buf(), m)])
                .await?
                .remove(0);
            if !matches!(result, Hashing::Changing) {
                break;
            }
        }
        let m = Meta::lstat(path)?;
        match result {
            Hashing::Changing => Err(fail(
                "changing",
                format!("{} keeps changing while being read", path.display()),
            )),
            Hashing::Failed(e) => {
                crate::say!(0, "verify {}: {e}", path.display());
                self.index.forget_content_id(&conn, m.dev, m.ino).await?;
                if let Some(root) = before {
                    for p in self.inode_paths(&conn, &m).await? {
                        self.lost(&root, &p, "unreadable");
                    }
                }
                answer("unreadable", None)
            }
            Hashing::Done(None) => {
                self.index.forget_content_id(&conn, m.dev, m.ino).await?;
                if let Some(root) = before {
                    for p in self.inode_paths(&conn, &m).await? {
                        self.lost(&root, &p, "changed");
                    }
                }
                answer("changed", None)
            }
            Hashing::Done(Some(h)) => {
                let now = h.id.root;
                if before.as_deref().is_some_and(|b| b != now.as_slice()) {
                    let old = before.clone().unwrap_or_default();
                    for p in self.inode_paths(&conn, &m).await? {
                        self.lost(&old, &p, "changed");
                    }
                }
                self.record_hash(&conn, path, &m, &h).await?;
                if now == claimed {
                    answer("unchanged", Some(content_hex(&now)))
                } else {
                    crate::say!(
                        0,
                        "verify {}: holds {}, not {}",
                        path.display(),
                        content_hex(&now),
                        content_hex(&claimed)
                    );
                    answer("changed", Some(content_hex(&now)))
                }
            }
        }
    }

    async fn inode_paths(&self, conn: &turso::Connection, m: &Meta) -> Result<Vec<PathBuf>> {
        let mut cache = HashMap::new();
        let mut out = Vec::new();
        for e in self.index.inode_entries(conn, m.dev, m.ino).await? {
            out.push(self.index.path_of(conn, e, &mut cache).await?);
        }
        Ok(out)
    }

    fn lost(&self, root: &[u8], path: &Path, reason: &str) {
        self.events.emit(
            "content.lost",
            json!({ "id": content_hex(root), "path": path, "reason": reason }),
        );
    }

    /// Store a fresh hash of `path`, link its entry and announce it if the
    /// path was not already an observation of that content.
    async fn record_hash(
        &self,
        conn: &turso::Connection,
        path: &Path,
        m: &Meta,
        h: &steward_contentid::Hashed,
    ) -> Result<()> {
        let before = self.index.fresh_content_id(conn, m).await?;
        self.save_hash(conn, m, h).await?;
        let mut paths = Vec::new();
        let mut cache = HashMap::new();
        if let Some(entry) = self.index.resolve(conn, path).await? {
            self.index.link_content(conn, entry, m).await?;
        }
        for e in self.index.inode_entries(conn, m.dev, m.ino).await? {
            paths.push(self.index.path_of(conn, e, &mut cache).await?);
        }
        if before.as_deref() != Some(h.id.root.as_slice()) {
            for p in paths {
                self.events.emit(
                    "content.observed",
                    json!({ "id": content_hex(&h.id.root), "path": p }),
                );
            }
        }
        Ok(())
    }

    /// Hash `files` now on the hashing pool, in order.
    async fn hash_now(&self, files: Vec<(PathBuf, Meta)>) -> Result<Vec<Hashing>> {
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let pool = std::sync::Arc::clone(
            &self
                .hash_pool
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        Ok(tokio::task::spawn_blocking(move || {
            use rayon::prelude::*;
            pool.install(|| {
                files
                    .par_iter()
                    .map(|(p, m)| match steward_contentid::hash_file(p) {
                        Ok(_) if Meta::lstat(p).ok().as_ref() != Some(m) => Hashing::Changing,
                        Ok(h) => Hashing::Done(h),
                        Err(e) => Hashing::Failed(format!("{}: {e}", p.display())),
                    })
                    .collect()
            })
        })
        .await?)
    }

    /// Announce what a scan changed: content observed, lost or moved, and
    /// volumes found offline or back.
    fn publish_scan(&self, stats: &ScanStats, was_offline: &[PathBuf]) {
        let mut observed: HashMap<(u64, u64, &[u8]), Vec<&Path>> = HashMap::new();
        for c in &stats.content {
            if c.kind == ChangeKind::Observed {
                observed
                    .entry((c.dev, c.ino, &c.root))
                    .or_default()
                    .push(&c.path);
            }
        }
        for c in &stats.content {
            match c.kind {
                ChangeKind::Deleted => {
                    match observed
                        .get_mut(&(c.dev, c.ino, c.root.as_slice()))
                        .and_then(Vec::pop)
                    {
                        Some(to) => self.events.emit(
                            "content.moved",
                            json!({ "id": content_hex(&c.root), "from": c.path, "to": to }),
                        ),
                        None => self.lost(&c.root, &c.path, "deleted"),
                    }
                }
                ChangeKind::Changed => self.lost(&c.root, &c.path, "changed"),
                ChangeKind::Observed => {}
            }
        }
        for ((_, _, root), paths) in observed {
            for p in paths {
                self.events.emit(
                    "content.observed",
                    json!({ "id": content_hex(root), "path": p }),
                );
            }
        }
        for p in &stats.offline {
            if !was_offline.contains(p) {
                self.events.emit("storage.offline", json!({ "path": p }));
            }
        }
        for p in was_offline {
            if !stats.offline.contains(p) {
                self.events.emit("storage.online", json!({ "path": p }));
            }
        }
    }

    async fn stored_id(&self, conn: &turso::Connection, r: &Record) -> Result<Option<String>> {
        if r.meta.kind != Kind::File || r.meta.size == 0 {
            return Ok(None);
        }
        let root = self.index.fresh_content_id(conn, &r.meta).await?;
        Ok(root.map(|h| format!("btv2:{}", steward_contentid::hex(&h))))
    }

    async fn settings(&self) -> Result<Value> {
        let config = self.config();
        let conn = self.index.connect()?;
        let mut roots = Vec::new();
        for r in &config.roots {
            let indexed = match self.index.resolve(&conn, &r.path).await? {
                Some(id) => self
                    .index
                    .get(&conn, id)
                    .await?
                    .map(|rec| entry(&rec, &r.path, vec![])),
                None => None,
            };
            let offline = self
                .offline
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .any(|p| p == &r.path);
            roots.push(json!({ "settings": r, "indexed": indexed, "offline": offline }));
        }
        Ok(json!({
            "file": Config::path(),
            "db": config.db,
            "roots": roots,
            "scanning": self.scan_lock.try_lock().is_err(),
            "hashing": self.hash_progress(),
            "hash_threads": config.hash_threads(),
            "hash_threads_default": crate::config::default_hash_threads(),
        }))
    }

    /// How much of `path` has content ids, and how much of it is duplicated.
    async fn content_summary(&self, path: &Path) -> Result<Value> {
        // Enough for any media library; bounds the cost if pointed at `/`.
        const MAX_FILES: u64 = 2_000_000;
        let conn = self.index.connect()?;
        let top = self.resolve(&conn, path).await?;
        let (mut files, mut bytes, mut hashed, mut hashed_bytes) = (0u64, 0u64, 0u64, 0u64);
        let mut ids: HashMap<Vec<u8>, (u64, u64)> = HashMap::new();
        let mut stack = vec![top];
        let mut truncated = false;
        'walk: while let Some(dir) = stack.pop() {
            for r in self.index.children_of(&conn, dir).await? {
                match r.meta.kind {
                    Kind::Dir => stack.push(r.id),
                    Kind::File => {
                        files += 1;
                        bytes += r.meta.size;
                        if let Some(root) = self.index.fresh_content_id(&conn, &r.meta).await? {
                            hashed += 1;
                            hashed_bytes += r.meta.size;
                            let e = ids.entry(root).or_insert((0, r.meta.size));
                            e.0 += 1;
                        }
                        if files >= MAX_FILES {
                            truncated = true;
                            break 'walk;
                        }
                    }
                    _ => {}
                }
            }
        }
        let dups: Vec<_> = ids.values().filter(|(n, _)| *n > 1).collect();
        Ok(json!({
            "path": path,
            "files": files,
            "bytes": bytes,
            "hashed_files": hashed,
            "hashed_bytes": hashed_bytes,
            "unhashed_files": files - hashed,
            "unhashed_bytes": bytes - hashed_bytes,
            "distinct_ids": ids.len(),
            "duplicate_groups": dups.len(),
            "duplicate_files": dups.iter().map(|(n, _)| n).sum::<u64>(),
            "wasted_bytes": dups.iter().map(|(n, s)| (n - 1) * s).sum::<u64>(),
            "truncated": truncated,
        }))
    }

    pub async fn resolve(&self, conn: &turso::Connection, path: &Path) -> Result<i64> {
        if let Some(id) = self.index.resolve(conn, path).await? {
            return Ok(id);
        }
        let config = self.config();
        let p = path.display();
        match config.root_for(path) {
            Some(r) if self.index.resolve(conn, &r.path).await?.is_some() => Err(fail(
                "not_indexed",
                format!(
                    "{p} is not in the index: nothing by that name under root {}",
                    r.path.display()
                ),
            )),
            Some(r) => Err(fail(
                "not_indexed",
                format!(
                    "{p} is not indexed yet: root {} has not finished its first scan",
                    r.path.display()
                ),
            )),
            None => Err(fail(
                "not_indexed",
                format!("{p} is not indexed: no configured root covers it"),
            )),
        }
    }

    /// Rescan, then bring the derived layers up to date for that subtree.
    pub async fn refresh(&self, path: &Path, trust_dir_mtime: bool) -> Result<ScanReport> {
        self.refresh_with(path, trust_dir_mtime, false).await
    }

    /// `refresh`, optionally reading `path`'s own listing even when trusting.
    pub async fn refresh_with(
        &self,
        path: &Path,
        trust_dir_mtime: bool,
        read_root: bool,
    ) -> Result<ScanReport> {
        let mut opts = self.scan_options(path, trust_dir_mtime);
        opts.read_root = read_root;
        let kind = if trust_dir_mtime { "trusting" } else { "full" };
        if self.scan_lock.try_lock().is_err() {
            crate::say!(
                1,
                "{kind} scan of {} waiting for the scan in progress",
                path.display()
            );
        }
        let guard = self.scan_lock.lock().await;
        crate::say!(
            1,
            "{kind} scan of {} started (excludes: {:?})",
            path.display(),
            opts.exclude
        );
        let stats = self.index.scan(path, opts).await?;
        for p in &stats.offline {
            crate::say!(
                0,
                "{} is offline (its volume is not mounted); left as indexed",
                p.display()
            );
        }
        let was_offline = {
            let mut offline = self
                .offline
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (under, rest): (Vec<_>, Vec<_>) =
                offline.drain(..).partition(|p| p.starts_with(&stats.root));
            *offline = rest;
            offline.extend(stats.offline.iter().cloned());
            under
        };
        self.publish_scan(&stats, &was_offline);
        crate::say!(
            1,
            "{kind} scan of {} done in {} ms: read {} dirs, trusted {}, {} errors; load {} ms, \
             walk+write {} ms, totals {} ms ({} dirs); {}",
            stats.root.display(),
            stats.millis,
            stats.dirs_read,
            stats.dirs_trusted,
            stats.errors,
            stats.load_ms,
            stats.write_ms,
            stats.totals_ms,
            stats.retotalled,
            steward_index::rss()
        );
        let config = self.config();
        if config.root_for(&stats.root).is_none_or(|r| r.classify) {
            let t = std::time::Instant::now();
            match self.classify(&stats.root).await {
                Ok(s) => crate::say!(
                    1,
                    "classified {}: {} entries, {} tags in {} ms (load {} ms, rules {} ms, \
                     write {} ms); {}",
                    stats.root.display(),
                    s.scanned,
                    s.tagged,
                    t.elapsed().as_millis(),
                    s.load_ms,
                    s.rules_ms,
                    s.write_ms,
                    steward_index::rss()
                ),
                Err(e) => crate::say!(0, "classify {}: {e:#}", stats.root.display()),
            }
        }
        drop(guard);
        for target in config.content_id_targets(&stats.root) {
            match &self.hash_queue {
                Some(tx) => {
                    let fresh = self
                        .hash_waiting
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(target.clone());
                    if fresh {
                        crate::say!(1, "content ids queued for {}", target.display());
                        let _ = tx.send(target);
                    } else {
                        crate::say!(1, "content ids for {} already queued", target.display());
                    }
                }
                None => {
                    if let Err(e) = self.hash_tree(&target).await {
                        crate::say!(0, "hash {}: {e:#}", target.display());
                    }
                }
            }
        }
        Ok(report(&stats))
    }

    /// Classification needs a repository's ignore rules, so classify from the
    /// enclosing repository; below an already-classified directory there is
    /// nothing to do.
    pub async fn classify(&self, path: &Path) -> Result<steward_classify::Summary> {
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

    pub async fn content_id(
        &self,
        conn: &turso::Connection,
        path: &Path,
        m: &Meta,
    ) -> Result<Option<ContentId>> {
        if m.kind != Kind::File {
            bail!("{} is not a regular file", path.display());
        }
        if let Some(root) = self.index.fresh_content_id(conn, m).await?
            && self.has_layer(conn, &root, m.size).await?
        {
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
        if let Some(h) = &cid {
            self.record_hash(conn, path, m, h).await?;
        }
        Ok(cid.map(|h| h.id))
    }

    /// Content known to be hashed also has its verification layer, unless
    /// the file is too small to have one.
    async fn has_layer(&self, conn: &turso::Connection, root: &[u8], size: u64) -> Result<bool> {
        Ok(size <= steward_contentid::LAYER_PIECE || self.index.layer(conn, root).await?.is_some())
    }

    async fn save_hash(
        &self,
        conn: &turso::Connection,
        m: &Meta,
        h: &steward_contentid::Hashed,
    ) -> Result<()> {
        self.index.put_content_id(conn, m, &h.id.root).await?;
        if !h.layer.is_empty() {
            self.index
                .put_layer(conn, &h.id.root, h.id.size, &h.layer.concat())
                .await?;
        }
        Ok(())
    }

    /// The BEP-52 piece layer of content `id` for `piece_size`, derived from
    /// the stored 1 MiB layer without reading the file.
    async fn piece_layer(&self, id: &str, piece_size: u64) -> Result<Value> {
        let cid = ContentId {
            root: parse_id(id)?,
            size: 0,
        };
        let conn = self.index.connect()?;
        let (size, layer) = match self.index.layer(&conn, &cid.root).await? {
            Some((size, bytes)) => {
                let layer: Vec<[u8; 32]> = bytes
                    .chunks_exact(32)
                    .map(|c| c.try_into().expect("32 bytes"))
                    .collect();
                (size, layer)
            }
            None => match self.index.content_size(&conn, &cid.root).await? {
                // Too small to have one: BEP 52 gives it no piece layer.
                Some(size) if size <= steward_contentid::LAYER_PIECE => (size, Vec::new()),
                Some(_) => {
                    return Err(fail(
                        "no_layer",
                        format!("no verification layer stored for {id}; inspect a copy of it"),
                    ));
                }
                None => return Err(fail("unknown_content", format!("unknown content {id}"))),
            },
        };
        let hashes = steward_contentid::piece_layer(&layer, size, piece_size).ok_or_else(|| {
            fail(
                "invalid_params",
                format!(
                    "piece size must be a power of two of at least {}",
                    steward_contentid::LAYER_PIECE
                ),
            )
        })?;
        Ok(json!({
            "id": format!("btv2:{}", cid.to_hex()),
            "size": size,
            "piece_size": piece_size,
            "layer": hashes.iter().map(|h| steward_contentid::hex(h)).collect::<String>(),
        }))
    }

    pub async fn hash_tree(&self, path: &Path) -> Result<Value> {
        let conn = self.index.connect()?;
        let id = self.resolve(&conn, path).await?;
        let mut stale = Vec::new();
        let mut current = Vec::new();
        for (r, p) in self.index.subtree(&conn, id, path).await? {
            if r.meta.kind != Kind::File || r.meta.size == 0 {
                continue;
            }
            let hashed = match self.index.fresh_content_id(&conn, &r.meta).await? {
                Some(root) => self.has_layer(&conn, &root, r.meta.size).await?,
                None => false,
            };
            if !hashed {
                stale.push((p, r.meta, r.id));
            } else {
                current.push((r.id, r.meta));
            }
        }
        // Already-hashed files may have been renamed (new entry, same inode)
        // since they were linked; relinking is cheap and keeps lookups exact.
        conn.execute("BEGIN", ()).await?;
        for (entry, m) in &current {
            self.index.link_content(&conn, *entry, m).await?;
        }
        conn.execute("COMMIT", ()).await?;
        let files_total = stale.len() as u64;
        let bytes_total: u64 = stale.iter().map(|(_, m, _)| m.size).sum();
        crate::say!(
            1,
            "hashing {files_total} files ({}) under {}",
            gib(bytes_total),
            path.display()
        );
        let started = std::time::Instant::now();
        self.set_hash_progress(Some(HashProgress {
            path: path.to_path_buf(),
            files_total,
            bytes_total,
            files_done: 0,
            bytes_done: 0,
        }));

        // Results stream back as each file finishes, so a restart loses at
        // most the files in flight.
        type Done = (i64, PathBuf, Meta, Option<steward_contentid::Hashed>);
        let (tx, mut rx) = mpsc::channel::<Done>(256);
        let pool = std::sync::Arc::clone(
            &self
                .hash_pool
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let walk = tokio::task::spawn_blocking(move || {
            use rayon::prelude::*;
            let _ = pool.install(|| {
                stale
                    .into_par_iter()
                    .try_for_each_with(tx, |tx, (p, m, entry)| {
                        let t = std::time::Instant::now();
                        let result = steward_contentid::hash_file(&p);
                        let changed = Meta::lstat(&p).ok() != Some(m);
                        let secs = t.elapsed().as_secs_f64();
                        match &result {
                            Ok(_) if changed => {
                                crate::say!(
                                    2,
                                    "hash {}: changed while reading, skipped",
                                    p.display()
                                );
                            }
                            Ok(_) => crate::say!(
                                2,
                                "hashed {} ({}) in {secs:.1} s, {:.0} MB/s",
                                p.display(),
                                gib(m.size),
                                m.size as f64 / secs.max(1e-3) / 1e6
                            ),
                            Err(e) => crate::say!(1, "hash {}: {e}", p.display()),
                        }
                        let cid = result.ok().flatten().filter(|_| !changed);
                        // The job is gone: stop reading instead of discarding.
                        tx.blocking_send((entry, p.clone(), m, cid)).map_err(|_| ())
                    })
            });
        });

        let (mut files_done, mut bytes_done, mut hashed) = (0u64, 0u64, 0u64);
        let mut batch = Vec::new();
        let mut last_flush = std::time::Instant::now();
        let mut last_log = std::time::Instant::now();
        loop {
            let next = rx.recv().await;
            let done = next.is_none();
            if let Some((entry, p, m, cid)) = next {
                files_done += 1;
                bytes_done += m.size;
                if let Some(c) = cid {
                    batch.push((entry, p, m, c));
                }
            }
            if !batch.is_empty()
                && (done || batch.len() >= 64 || last_flush.elapsed().as_secs() >= 5)
            {
                let n = batch.len();
                let seen = self.save_batch(&conn, &batch).await?;
                hashed += batch.len() as u64;
                batch.clear();
                for (p, root) in seen {
                    self.events.emit(
                        "content.observed",
                        json!({ "id": content_hex(&root), "path": p }),
                    );
                }
                crate::say!(
                    2,
                    "saved {n} content ids ({hashed} so far) under {}",
                    path.display()
                );
                last_flush = std::time::Instant::now();
            }
            self.set_hash_progress(Some(HashProgress {
                path: path.to_path_buf(),
                files_total,
                bytes_total,
                files_done,
                bytes_done,
            }));
            if last_log.elapsed().as_secs() >= 60 || done {
                last_log = std::time::Instant::now();
                let secs = started.elapsed().as_secs_f64().max(1.0);
                let rate = bytes_done as f64 / secs;
                let eta = if rate > 0.0 {
                    (bytes_total - bytes_done) as f64 / rate
                } else {
                    0.0
                };
                crate::say!(
                    1,
                    "hashing {}: {files_done}/{files_total} files, {} of {}, {:.0} MB/s, {} left",
                    path.display(),
                    gib(bytes_done),
                    gib(bytes_total),
                    rate / 1e6,
                    duration(eta)
                );
            }
            if done {
                break;
            }
        }
        walk.await?;
        self.set_hash_progress(None);
        Ok(json!({ "stale": files_total, "hashed": hashed }))
    }

    /// Store a batch of hashes in one transaction; returns the paths that
    /// were not already observations of their content. Retried, because
    /// another connection's commit can invalidate it, and one lost batch
    /// must not end a job that takes days.
    async fn save_batch(
        &self,
        conn: &turso::Connection,
        batch: &[(i64, PathBuf, Meta, steward_contentid::Hashed)],
    ) -> Result<Vec<(PathBuf, [u8; 32])>> {
        // Reads first: a transaction that reads before writing goes stale
        // when a scan commits in between.
        let mut seen = Vec::new();
        for (_, p, m, h) in batch {
            let before = self.index.fresh_content_id(conn, m).await?;
            if before.as_deref() != Some(h.id.root.as_slice()) {
                seen.push((p.clone(), h.id.root));
            }
        }
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let result: Result<()> = async {
                conn.execute("BEGIN", ()).await?;
                for (entry, _, m, h) in batch {
                    self.save_hash(conn, m, h).await?;
                    self.index.link_content(conn, *entry, m).await?;
                }
                conn.execute("COMMIT", ()).await?;
                Ok(())
            }
            .await;
            match result {
                Ok(()) => return Ok(seen),
                Err(e) => {
                    let _ = conn.execute("ROLLBACK", ()).await;
                    if attempt >= 10 {
                        return Err(e);
                    }
                    crate::say!(1, "saving content ids: {e:#}; retrying");
                    tokio::time::sleep(std::time::Duration::from_millis(250 * u64::from(attempt)))
                        .await;
                }
            }
        }
    }

    /// The hash worker took `target` off the queue; a later scan may queue
    /// it again to catch files added while this job runs.
    pub fn hash_started(&self, target: &Path) {
        self.hash_waiting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(target);
    }

    fn set_hash_progress(&self, p: Option<HashProgress>) {
        *self
            .hashing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = p;
    }

    pub fn hash_progress(&self) -> Option<HashProgress> {
        self.hashing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Content stored at more than one path, as far as those paths lie under
    /// `path`. Groups come from the content-id table, not a tree walk, so
    /// this stays cheap on any root and finds copies across folders.
    pub async fn duplicates(&self, path: &Path, limit: u32) -> Result<Value> {
        let conn = self.index.connect()?;
        self.resolve(&conn, path).await?;
        let mut dups = Vec::new();
        let mut cache = HashMap::new();
        for (root, size) in self.index.shared_content_ids(&conn).await? {
            let mut paths = Vec::new();
            for id in self.index.find_content(&conn, &root).await? {
                let p = self.index.path_of(&conn, id, &mut cache).await?;
                if p.starts_with(path) {
                    paths.push(p.display().to_string());
                }
            }
            if paths.len() > 1 {
                paths.sort();
                let wasted = size * (paths.len() as u64 - 1);
                dups.push((wasted, steward_contentid::hex(&root), size, paths));
            }
        }
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
}
