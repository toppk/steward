//! Layer 1: the filesystem index. One row per path with its `lstat` fields,
//! plus subtree totals on directories, kept current by rescans that only
//! write what changed.

pub mod scan;

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use ignore::gitignore::GitignoreBuilder;
use turso::{Connection, Database, Row, Value};

use scan::{Counters, KnownDir, Listing, Snapshot, Walker};
pub use scan::{Kind, Meta};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS entries (
    id INTEGER PRIMARY KEY,
    parent INTEGER NOT NULL,
    name BLOB NOT NULL,
    kind INTEGER NOT NULL,
    mode INTEGER NOT NULL,
    uid INTEGER NOT NULL,
    gid INTEGER NOT NULL,
    size INTEGER NOT NULL,
    alloc INTEGER NOT NULL,
    nlink INTEGER NOT NULL,
    dev INTEGER NOT NULL,
    ino INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    ctime_ns INTEGER NOT NULL,
    t_size INTEGER NOT NULL DEFAULT 0,
    t_alloc INTEGER NOT NULL DEFAULT 0,
    t_files INTEGER NOT NULL DEFAULT 0,
    t_dirs INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX IF NOT EXISTS entries_parent_name ON entries(parent, name);
CREATE INDEX IF NOT EXISTS entries_dev_ino ON entries(dev, ino);
CREATE TABLE IF NOT EXISTS tags (
    entry INTEGER NOT NULL,
    source TEXT NOT NULL,
    tag TEXT NOT NULL,
    PRIMARY KEY (entry, source, tag)
);
CREATE INDEX IF NOT EXISTS tags_tag ON tags(tag);
CREATE TABLE IF NOT EXISTS contentids (
    dev INTEGER NOT NULL,
    ino INTEGER NOT NULL,
    size INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    ctime_ns INTEGER NOT NULL,
    root BLOB NOT NULL,
    PRIMARY KEY (dev, ino)
);
CREATE INDEX IF NOT EXISTS contentids_root ON contentids(root);
CREATE TABLE IF NOT EXISTS dirty (id INTEGER PRIMARY KEY);
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
";

/// Bumped whenever the meaning of the `t_*` columns changes; an index built
/// under another version gets every total recomputed on open.
const TOTALS_VERSION: i64 = 2;

/// Past this many directories to re-total, one pass over the whole table
/// beats a query per directory.
const FULL_AGGREGATE_OVER: usize = 50_000;

/// Secondary indexes on `entries`, as (name, definition); these must match
/// SCHEMA, which recreates any that are missing.
const BULK_INDEXES: [(&str, &str); 2] = [
    (
        "entries_parent_name",
        "CREATE UNIQUE INDEX IF NOT EXISTS entries_parent_name ON entries(parent, name);",
    ),
    (
        "entries_dev_ino",
        "CREATE INDEX IF NOT EXISTS entries_dev_ino ON entries(dev, ino);",
    ),
];

const ENTRY_COLS: &str = "id, parent, name, kind, mode, uid, gid, size, \
    alloc, nlink, dev, ino, mtime_ns, ctime_ns, t_size, t_alloc, t_files, t_dirs";

/// Writes per transaction during a scan, so readers see progress and a
/// crash loses little. Measured on disk: per-directory commits are 10x
/// slower; from 10k up the cost is flat, and one transaction for a whole
/// scan would hold the write lock for minutes.
const COMMIT_EVERY: u64 = 20_000;

#[derive(Clone, Debug)]
pub struct Record {
    pub id: i64,
    pub parent: i64,
    pub name: OsString,
    pub meta: Meta,
    pub t_size: u64,
    pub t_alloc: u64,
    pub t_files: u64,
    pub t_dirs: u64,
}

#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub trust_dir_mtime: bool,
    pub one_filesystem: bool,
    /// gitignore-style patterns relative to `exclude_base` (the configured
    /// root, which may lie above the path being scanned).
    pub exclude: Vec<String>,
    pub exclude_base: Option<PathBuf>,
    pub progress: Option<Progress>,
    /// Directories whose times fall within this long before the scan are
    /// stored as untrusted (mlocate's guard against same-tick changes).
    pub recent_window: std::time::Duration,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            trust_dir_mtime: false,
            one_filesystem: true,
            exclude: Vec::new(),
            exclude_base: None,
            progress: None,
            recent_window: std::time::Duration::from_secs(1),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ScanStats {
    pub root: PathBuf,
    pub dirs_read: u64,
    pub dirs_trusted: u64,
    pub entries_seen: u64,
    pub inserted: u64,
    pub updated: u64,
    pub deleted: u64,
    pub errors: u64,
    pub millis: u64,
    /// Reading what the index already holds for the tree.
    pub load_ms: u64,
    /// Walking and writing, which overlap.
    pub write_ms: u64,
    /// Recomputing directory totals afterwards.
    pub totals_ms: u64,
    pub retotalled: u64,
}

/// Receives human-readable progress lines during a scan.
#[derive(Clone)]
pub struct Progress(pub Arc<dyn Fn(String) + Send + Sync>);

impl std::fmt::Debug for Progress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Progress")
    }
}

impl Progress {
    fn say(opt: Option<&Self>, line: impl FnOnce() -> String) {
        if let Some(p) = opt {
            (p.0)(line());
        }
    }
}

/// Resident memory of this process, for progress lines ("rss 1.2 GiB").
pub fn rss() -> String {
    let pages = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| {
            s.split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok())
        })
        .unwrap_or(0);
    format!(
        "rss {:.2} GiB",
        (pages * 4096) as f64 / f64::from(1u32 << 30)
    )
}

/// How often a running scan reports progress.
const PROGRESS_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// An entry's place in the tree, without its metadata.
#[derive(Clone, Debug)]
pub struct Node {
    pub id: i64,
    pub parent: i64,
    pub name: Box<[u8]>,
    pub is_dir: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredContentId {
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub root: Vec<u8>,
}

#[derive(Debug)]
pub struct Index {
    db: Database,
}

fn int(row: &Row, i: usize) -> Result<i64> {
    match row.get_value(i)? {
        Value::Integer(v) => Ok(v),
        Value::Null => Ok(0),
        v => bail!("column {i}: expected integer, got {v:?}"),
    }
}

fn blob(row: &Row, i: usize) -> Result<Vec<u8>> {
    match row.get_value(i)? {
        Value::Blob(v) => Ok(v),
        Value::Text(t) => Ok(t.into_bytes()),
        v => bail!("column {i}: expected blob, got {v:?}"),
    }
}

/// Every statement goes through the connection's statement cache: compiling
/// SQL was most of the cost of a scan, and a scan runs a few statements
/// millions of times.
async fn exec(
    conn: &Connection,
    sql: impl AsRef<str>,
    params: impl turso::IntoParams,
) -> Result<u64> {
    Ok(conn.prepare_cached(sql).await?.execute(params).await?)
}

async fn query(
    conn: &Connection,
    sql: impl AsRef<str>,
    params: impl turso::IntoParams,
) -> Result<turso::Rows> {
    Ok(conn.prepare_cached(sql).await?.query(params).await?)
}

fn record(row: &Row) -> Result<Record> {
    let u = |i| int(row, i).map(|v| v as u64);
    Ok(Record {
        id: int(row, 0)?,
        parent: int(row, 1)?,
        name: OsString::from_vec(blob(row, 2)?),
        meta: Meta {
            kind: Kind::from_i64(int(row, 3)?),
            mode: u(4)? as u32,
            uid: u(5)? as u32,
            gid: u(6)? as u32,
            size: u(7)?,
            alloc: u(8)?,
            nlink: u(9)?,
            dev: u(10)?,
            ino: u(11)?,
            mtime_ns: int(row, 12)?,
            ctime_ns: int(row, 13)?,
        },
        t_size: u(14)?,
        t_alloc: u(15)?,
        t_files: u(16)?,
        t_dirs: u(17)?,
    })
}

fn name_bytes(name: &OsStr) -> Vec<u8> {
    name.as_bytes().to_vec()
}

/// Totals a non-directory contributes to its ancestors. A hardlinked file
/// contributes `1/nlink` per link, so totals stay right without a global
/// de-duplication pass whenever all links sit inside the subtree.
fn leaf_totals(m: &Meta) -> (i64, i64, i64, i64) {
    let n = m.nlink.max(1);
    // "Files" means regular files, as in qdirstat; links and devices aren't.
    (
        (m.size / n) as i64,
        (m.alloc / n) as i64,
        i64::from(m.kind == Kind::File),
        0,
    )
}

const INSERT_COLS: usize = 18;

/// Rows per INSERT statement; one statement per row spent most of a first
/// scan in per-statement setup.
const INSERT_BATCH: usize = 256;

/// Buffers new rows and writes them many per statement. Ids come from a
/// counter shared with the walker (which numbers new directories), not from
/// the database; only one scan writes at a time, so they cannot collide.
struct Inserter {
    next_id: Arc<AtomicI64>,
    first_id: i64,
    rows: usize,
    params: Vec<Value>,
}

impl Inserter {
    async fn new(conn: &Connection) -> Result<Self> {
        // turso answers max(id) by scanning every row; this form is instant.
        let mut rows = query(conn, "SELECT id FROM entries ORDER BY id DESC LIMIT 1", ()).await?;
        let max = match rows.next().await? {
            Some(row) => int(&row, 0)?,
            None => 0,
        };
        Ok(Self {
            next_id: Arc::new(AtomicI64::new(max + 1)),
            first_id: max + 1,
            rows: 0,
            params: Vec::with_capacity(INSERT_BATCH * INSERT_COLS),
        })
    }

    async fn add(
        &mut self,
        conn: &Connection,
        parent: i64,
        name: Vec<u8>,
        m: &Meta,
    ) -> Result<i64> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.add_with_id(conn, id, parent, name, m).await?;
        Ok(id)
    }

    async fn add_with_id(
        &mut self,
        conn: &Connection,
        id: i64,
        parent: i64,
        name: Vec<u8>,
        m: &Meta,
    ) -> Result<()> {
        let (ts, ta, tf, td) = if m.kind == Kind::Dir {
            (0, 0, 0, 1)
        } else {
            leaf_totals(m)
        };
        self.params.extend([
            Value::Integer(id),
            Value::Integer(parent),
            Value::Blob(name),
            Value::Integer(m.kind as i64),
            Value::Integer(m.mode.into()),
            Value::Integer(m.uid.into()),
            Value::Integer(m.gid.into()),
            Value::Integer(m.size as i64),
            Value::Integer(m.alloc as i64),
            Value::Integer(m.nlink as i64),
            Value::Integer(m.dev as i64),
            Value::Integer(m.ino as i64),
            Value::Integer(m.mtime_ns),
            Value::Integer(m.ctime_ns),
            Value::Integer(ts),
            Value::Integer(ta),
            Value::Integer(tf),
            Value::Integer(td),
        ]);
        self.rows += 1;
        if self.rows == INSERT_BATCH {
            self.flush(conn).await?;
        }
        Ok(())
    }

    async fn flush(&mut self, conn: &Connection) -> Result<()> {
        if self.rows == 0 {
            return Ok(());
        }
        let row = format!("({})", vec!["?"; INSERT_COLS].join(","));
        let sql = format!(
            "INSERT INTO entries (id, parent, name, kind, mode, uid, gid, size, alloc, nlink, \
             dev, ino, mtime_ns, ctime_ns, t_size, t_alloc, t_files, t_dirs) VALUES {}",
            vec![row.as_str(); self.rows].join(",")
        );
        exec(conn, sql, std::mem::take(&mut self.params)).await?;
        self.rows = 0;
        Ok(())
    }
}

/// mlocate's rule: a directory whose time is within a second of the scan
/// could change again inside the same timestamp tick, so it is stored as
/// untrusted (ctime 0) and the next trusting scan rereads it.
fn guarded(m: &Meta, recent_ns: i64) -> Meta {
    let mut m = *m;
    if m.kind == Kind::Dir && m.mtime_ns.max(m.ctime_ns) >= recent_ns {
        m.ctime_ns = 0;
    }
    m
}

async fn update_meta(conn: &Connection, id: i64, m: &Meta) -> Result<()> {
    let mut params = vec![
        Value::Integer(m.mode.into()),
        Value::Integer(m.uid.into()),
        Value::Integer(m.gid.into()),
        Value::Integer(m.size as i64),
        Value::Integer(m.alloc as i64),
        Value::Integer(m.nlink as i64),
        Value::Integer(m.dev as i64),
        Value::Integer(m.ino as i64),
        Value::Integer(m.mtime_ns),
        Value::Integer(m.ctime_ns),
        Value::Integer(id),
    ];
    let sql = if m.kind == Kind::Dir {
        "UPDATE entries SET mode=?1, uid=?2, gid=?3, size=?4, alloc=?5, \
         nlink=?6, dev=?7, ino=?8, mtime_ns=?9, ctime_ns=?10 WHERE id=?11"
    } else {
        let (ts, ta, _, _) = leaf_totals(m);
        params.push(Value::Integer(ts));
        params.push(Value::Integer(ta));
        "UPDATE entries SET mode=?1, uid=?2, gid=?3, size=?4, alloc=?5, \
         nlink=?6, dev=?7, ino=?8, mtime_ns=?9, ctime_ns=?10, t_size=?12, \
         t_alloc=?13 WHERE id=?11"
    };
    exec(conn, sql, params).await?;
    Ok(())
}

/// Directories whose totals need recomputing, and which of those are not
/// yet recorded in the `dirty` table.
#[derive(Default)]
struct Dirty {
    set: HashSet<i64>,
    unsaved: Vec<i64>,
}

impl Dirty {
    fn mark(&mut self, id: i64) {
        if self.set.insert(id) {
            self.unsaved.push(id);
        }
    }
}

/// The single writer's state during a scan.
struct Writer {
    snapshot: Arc<Snapshot>,
    /// Former roots below the scan root: they are re-parented, not re-read.
    adopt: HashSet<i64>,
    root_id: i64,
    root_name: Vec<u8>,
    recent_ns: i64,
    /// Parent of every directory this scan knows, for re-totalling.
    parents: HashMap<i64, i64>,
    dirty: Dirty,
    ins: Inserter,
    stats: ScanStats,
}

impl Index {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let path = path.to_str().context("database path must be UTF-8")?;
        let db = turso::Builder::new_local(path).build().await?;
        let conn = db.connect()?;
        conn.execute_batch(SCHEMA).await?;
        let mut rows = query(&conn, "SELECT value FROM meta WHERE key = 'totals'", ()).await?;
        let version = match rows.next().await? {
            Some(row) => int(&row, 0)?,
            None => 0,
        };
        drop(rows);
        if version != TOTALS_VERSION {
            exec(&conn, "BEGIN", ()).await?;
            Self::full_aggregate(&conn).await?;
            exec(
                &conn,
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('totals', ?1)",
                (TOTALS_VERSION,),
            )
            .await?;
            exec(&conn, "COMMIT", ()).await?;
        }
        Ok(Self { db })
    }

    pub fn connect(&self) -> Result<Connection> {
        let conn = self.db.connect()?;
        // Scans, hashing and requests write concurrently; wait for the
        // single writer slot instead of failing with "database is locked".
        conn.busy_timeout(std::time::Duration::from_secs(60))?;
        Ok(conn)
    }

    /// Directories under `id` (inclusive) as the index holds them, added to
    /// `snap`. Big subtrees are read in one pass over the directory rows;
    /// small ones by walking down the `(parent, name)` index, so the cost of a
    /// scan's setup follows the size of what is being scanned.
    async fn load_known_under(
        &self,
        conn: &Connection,
        id: i64,
        snap: &mut Snapshot,
    ) -> Result<()> {
        let Some(top) = self.get(conn, id).await? else {
            return Ok(());
        };
        snap.insert(
            id,
            KnownDir {
                parent: top.parent,
                mtime_ns: top.meta.mtime_ns,
                ctime_ns: top.meta.ctime_ns,
                subdirs: Vec::new(),
            },
        );
        let mut queue = vec![id];
        if self.mostly_everything(conn, top.t_dirs).await? {
            // parent -> (id, name, mtime, ctime) of each directory row.
            type DirRow = (i64, Box<[u8]>, i64, i64);
            let mut kids: HashMap<i64, Vec<DirRow>> = HashMap::new();
            let mut rows = query(
                conn,
                "SELECT id, parent, name, mtime_ns, ctime_ns FROM entries WHERE kind = 1",
                (),
            )
            .await?;
            while let Some(row) = rows.next().await? {
                kids.entry(int(&row, 1)?).or_default().push((
                    int(&row, 0)?,
                    blob(&row, 2)?.into(),
                    int(&row, 3)?,
                    int(&row, 4)?,
                ));
            }
            while let Some(dir) = queue.pop() {
                for (cid, name, mt, ct) in kids.remove(&dir).unwrap_or_default() {
                    if let Some(k) = snap.get_mut(&dir) {
                        k.subdirs.push((name, cid));
                    }
                    snap.insert(
                        cid,
                        KnownDir {
                            parent: dir,
                            mtime_ns: mt,
                            ctime_ns: ct,
                            subdirs: Vec::new(),
                        },
                    );
                    queue.push(cid);
                }
            }
        } else {
            while let Some(dir) = queue.pop() {
                let mut rows = query(
                    conn,
                    "SELECT id, name, mtime_ns, ctime_ns FROM entries WHERE parent = ?1 AND kind = 1",
                    (dir,),
                )
                .await?;
                let mut found = Vec::new();
                while let Some(row) = rows.next().await? {
                    found.push((int(&row, 0)?, blob(&row, 1)?, int(&row, 2)?, int(&row, 3)?));
                }
                for (cid, name, mt, ct) in found {
                    if let Some(k) = snap.get_mut(&dir) {
                        k.subdirs.push((name.into(), cid));
                    }
                    snap.insert(
                        cid,
                        KnownDir {
                            parent: dir,
                            mtime_ns: mt,
                            ctime_ns: ct,
                            subdirs: Vec::new(),
                        },
                    );
                    queue.push(cid);
                }
            }
        }
        Ok(())
    }

    /// Bring the index under `root` up to date with the filesystem.
    pub async fn scan(&self, root: &Path, opts: ScanOptions) -> Result<ScanStats> {
        let started = Instant::now();
        let scan_start_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as i64);
        let root =
            std::fs::canonicalize(root).with_context(|| format!("resolving {}", root.display()))?;
        let conn = self.connect()?;
        let progress = opts.progress.clone();

        // Where the scan root sits in the index.
        let known_root = self.resolve(&conn, &root).await?;
        let (root_parent, root_name) = match known_root {
            Some(id) => {
                let r = self.get(&conn, id).await?.context("root vanished")?;
                (r.parent, name_bytes(&r.name))
            }
            None => match root.parent() {
                Some(p) => match self.resolve(&conn, p).await? {
                    Some(pid) => (pid, name_bytes(root.file_name().unwrap_or_default())),
                    None => (0, name_bytes(root.as_os_str())),
                },
                None => (0, name_bytes(root.as_os_str())),
            },
        };

        // What the index holds under the root, and other roots below it.
        let mut snapshot = Snapshot::new();
        if let Some(id) = known_root {
            self.load_known_under(&conn, id, &mut snapshot).await?;
        }
        let mut adopt = HashSet::new();
        for r in self.roots(&conn).await? {
            let path = PathBuf::from(&r.name);
            if path == root || !path.starts_with(&root) {
                continue;
            }
            let parent = match path.parent() {
                Some(pp) => self.resolve(&conn, pp).await?,
                None => None,
            };
            match parent.filter(|p| snapshot.contains_key(p)) {
                Some(pid) => {
                    self.load_known_under(&conn, r.id, &mut snapshot).await?;
                    if let Some(k) = snapshot.get_mut(&r.id) {
                        k.parent = pid;
                    }
                    let name = name_bytes(path.file_name().unwrap_or_default());
                    if let Some(k) = snapshot.get_mut(&pid) {
                        k.subdirs.push((name.into(), r.id));
                    }
                    adopt.insert(r.id);
                }
                // Its parent is not indexed yet: rescan it as part of this tree.
                None => {
                    self.remove_root(&path).await?;
                }
            }
        }
        let load_ms = started.elapsed().as_millis() as u64;
        Progress::say(progress.as_ref(), || {
            format!(
                "loaded {} known directories in {load_ms} ms; {}",
                snapshot.len(),
                rss()
            )
        });

        let ins = Inserter::new(&conn).await?;
        // Loading an empty index: keeping secondary indexes current row by
        // row costs more than building them once at the end. A crash leaves
        // them missing, and `open` recreates them.
        let bulk = ins.first_id == 1;
        let root_id = known_root.unwrap_or_else(|| ins.next_id.fetch_add(1, Ordering::Relaxed));
        let snapshot = Arc::new(snapshot);
        let mut parents: HashMap<i64, i64> =
            snapshot.iter().map(|(id, k)| (*id, k.parent)).collect();
        parents.insert(root_id, root_parent);

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Listing>(4096);
        let counters = Arc::new(Counters::default());
        let exclude = if opts.exclude.is_empty() {
            None
        } else {
            let mut b = GitignoreBuilder::new(opts.exclude_base.as_deref().unwrap_or(&root));
            for pat in &opts.exclude {
                b.add_line(None, pat)
                    .with_context(|| format!("exclude pattern {pat:?}"))?;
            }
            Some(b.build()?)
        };
        let walker = Walker {
            exclude,
            snapshot: Arc::clone(&snapshot),
            next_id: Arc::clone(&ins.next_id),
            trust_dir_mtime: opts.trust_dir_mtime,
            one_filesystem: opts.one_filesystem,
            counters: Arc::clone(&counters),
            tx,
        };
        let walk_root = root.clone();
        let walk =
            tokio::task::spawn_blocking(move || walker.run(&walk_root, root_id, root_parent));

        let mut w = Writer {
            snapshot,
            adopt,
            root_id,
            root_name,
            recent_ns: scan_start_ns - opts.recent_window.as_nanos() as i64,
            parents,
            dirty: Dirty::default(),
            ins,
            stats: ScanStats {
                root: root.clone(),
                ..ScanStats::default()
            },
        };
        // Directories an interrupted scan changed but never re-totalled.
        let mut rows = query(&conn, "SELECT id FROM dirty", ()).await?;
        while let Some(row) = rows.next().await? {
            w.dirty.set.insert(int(&row, 0)?);
        }
        drop(rows);
        if bulk {
            for index in BULK_INDEXES {
                exec(&conn, format!("DROP INDEX IF EXISTS {}", index.0), ()).await?;
            }
        }
        let mut pending: u64 = 0;
        let write_started = Instant::now();
        let mut last_report = Instant::now();
        exec(&conn, "BEGIN", ()).await?;
        let result: Result<()> = async {
            while let Some(listing) = rx.recv().await {
                let before = w.stats.inserted + w.stats.updated + w.stats.deleted;
                self.apply(&conn, &listing, &mut w).await?;
                pending += w.stats.inserted + w.stats.updated + w.stats.deleted - before;
                if last_report.elapsed() >= PROGRESS_EVERY {
                    last_report = Instant::now();
                    Progress::say(progress.as_ref(), || {
                        format!(
                            "walked {} dirs ({} trusted), {} entries; wrote +{} ~{} -{}; {} \
                             directory listings waiting to be written; {}",
                            counters.dirs_read.load(Ordering::Relaxed),
                            counters.dirs_trusted.load(Ordering::Relaxed),
                            counters.entries.load(Ordering::Relaxed),
                            w.stats.inserted,
                            w.stats.updated,
                            w.stats.deleted,
                            rx.len(),
                            rss()
                        )
                    });
                }
                if pending >= COMMIT_EVERY {
                    // Record what still needs re-totalling in the same
                    // transaction as the changes, so no crash can lose it.
                    w.ins.flush(&conn).await?;
                    for id in w.dirty.unsaved.drain(..) {
                        exec(&conn, "INSERT OR IGNORE INTO dirty (id) VALUES (?1)", (id,)).await?;
                    }
                    exec(&conn, "COMMIT", ()).await?;
                    exec(&conn, "BEGIN", ()).await?;
                    pending = 0;
                }
            }
            w.ins.flush(&conn).await?;
            Ok(())
        }
        .await;
        if let Err(e) = result {
            counters.cancelled.store(true, Ordering::Relaxed);
            drop(rx);
            let _ = walk.await;
            let _ = exec(&conn, "ROLLBACK", ()).await;
            return Err(e);
        }
        walk.await?;
        if bulk {
            exec(&conn, "COMMIT", ()).await?;
            let t = Instant::now();
            for index in BULK_INDEXES {
                conn.execute_batch(index.1).await?;
            }
            Progress::say(progress.as_ref(), || {
                format!("built indexes in {} ms; {}", t.elapsed().as_millis(), rss())
            });
            exec(&conn, "BEGIN", ()).await?;
        }
        let write_ms = write_started.elapsed().as_millis() as u64;
        let mut stats = w.stats;
        Progress::say(progress.as_ref(), || {
            format!(
                "walk and writes done in {write_ms} ms: +{} ~{} -{}; {}",
                stats.inserted,
                stats.updated,
                stats.deleted,
                rss()
            )
        });
        let totals_started = Instant::now();
        // The root's ancestors (outside this scan) hold its old totals.
        let mut dirty = w.dirty;
        let mut up = root_parent;
        while up != 0 {
            dirty.mark(up);
            up = Self::parent_of(&conn, &mut w.parents, up)
                .await?
                .unwrap_or_default();
        }
        let retotalled = dirty.set.len() as u64;
        Progress::say(progress.as_ref(), || {
            format!("re-totalling {retotalled} directories")
        });
        Self::aggregate(&conn, &mut w.parents, dirty.set).await?;
        exec(&conn, "DELETE FROM dirty", ()).await?;
        exec(&conn, "COMMIT", ()).await?;
        stats.load_ms = load_ms;
        stats.write_ms = write_ms;
        stats.totals_ms = totals_started.elapsed().as_millis() as u64;
        stats.retotalled = retotalled;

        stats.dirs_read = counters.dirs_read.load(Ordering::Relaxed);
        stats.dirs_trusted = counters.dirs_trusted.load(Ordering::Relaxed);
        stats.entries_seen = counters.entries.load(Ordering::Relaxed);
        stats.errors = counters.errors.load(Ordering::Relaxed);
        stats.millis = started.elapsed().as_millis() as u64;
        Ok(stats)
    }

    async fn apply(&self, conn: &Connection, listing: &Listing, w: &mut Writer) -> Result<()> {
        let id = listing.id;
        if listing.known {
            let changed = w.snapshot.get(&id).is_some_and(|k| {
                k.mtime_ns != listing.meta.mtime_ns || k.ctime_ns != listing.meta.ctime_ns
            });
            if changed {
                update_meta(conn, id, &guarded(&listing.meta, w.recent_ns)).await?;
                w.stats.updated += 1;
            }
        } else if id == w.root_id {
            // Other new directories were written from their parent's listing.
            let m = guarded(&listing.meta, w.recent_ns);
            w.ins
                .add_with_id(conn, id, listing.parent, w.root_name.clone(), &m)
                .await?;
            w.stats.inserted += 1;
            w.dirty.mark(id);
        }
        let Some(entries) = &listing.entries else {
            return Ok(());
        };

        // A directory new to the index has no stored children to compare.
        let mut stored: HashMap<Vec<u8>, Record> = HashMap::new();
        if listing.known {
            for r in self.children_of(conn, id).await? {
                stored.insert(name_bytes(&r.name), r);
            }
        }
        let mut changed = false;
        for child in entries {
            let key = name_bytes(&child.name);
            let m = &child.meta;
            match stored.remove(&key) {
                Some(old) if old.meta.kind == m.kind => {
                    // A walked subdirectory updates its own row with its
                    // listing, so a stored mtime never runs ahead of the
                    // stored children and fools a later trusting scan.
                    let own_listing = m.kind == Kind::Dir && m.dev == listing.meta.dev;
                    if old.meta != *m && !own_listing {
                        update_meta(conn, old.id, &guarded(m, w.recent_ns)).await?;
                        w.stats.updated += 1;
                        changed = true;
                    }
                }
                other => {
                    if let Some(old) = other {
                        w.stats.deleted += Self::delete_subtree(conn, old.id).await?;
                    }
                    match child.dir_id {
                        Some(cid) if w.adopt.contains(&cid) => {
                            // A former root now reached from above.
                            exec(
                                conn,
                                "UPDATE entries SET parent=?1, name=?2 WHERE id=?3",
                                (id, key, cid),
                            )
                            .await?;
                            w.parents.insert(cid, id);
                            w.dirty.mark(cid);
                        }
                        Some(cid) => {
                            let gm = guarded(m, w.recent_ns);
                            w.ins.add_with_id(conn, cid, id, key, &gm).await?;
                            w.parents.insert(cid, id);
                            w.dirty.mark(cid);
                        }
                        None => {
                            w.ins.add(conn, id, key, m).await?;
                        }
                    }
                    w.stats.inserted += 1;
                    changed = true;
                }
            }
        }
        for old in stored.into_values() {
            w.stats.deleted += Self::delete_subtree(conn, old.id).await?;
            changed = true;
        }
        if changed {
            w.dirty.mark(id);
        }
        Ok(())
    }

    async fn delete_subtree(conn: &Connection, id: i64) -> Result<u64> {
        let mut stack = vec![id];
        let mut n = 0;
        while let Some(cur) = stack.pop() {
            let mut rows = query(conn, "SELECT id FROM entries WHERE parent = ?1", (cur,)).await?;
            while let Some(row) = rows.next().await? {
                stack.push(int(&row, 0)?);
            }
            exec(conn, "DELETE FROM entries WHERE id = ?1", (cur,)).await?;
            exec(conn, "DELETE FROM tags WHERE entry = ?1", (cur,)).await?;
            n += 1;
        }
        Ok(n)
    }

    /// A directory's parent, from the scan's map or else the index.
    async fn parent_of(
        conn: &Connection,
        parents: &mut HashMap<i64, i64>,
        id: i64,
    ) -> Result<Option<i64>> {
        if let Some(&p) = parents.get(&id) {
            return Ok(Some(p));
        }
        let mut rows = query(conn, "SELECT parent FROM entries WHERE id = ?1", (id,)).await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        let p = int(&row, 0)?;
        parents.insert(id, p);
        Ok(Some(p))
    }

    /// Recompute totals of `dirty` directories and all their ancestors,
    /// deepest first so each sums already-correct children.
    async fn aggregate(
        conn: &Connection,
        parents: &mut HashMap<i64, i64>,
        dirty: HashSet<i64>,
    ) -> Result<()> {
        let mut all: HashSet<i64> = HashSet::new();
        for id in dirty {
            let mut cur = id;
            while cur != 0 && all.insert(cur) {
                cur = Self::parent_of(conn, parents, cur).await?.unwrap_or(0);
            }
        }
        if all.len() > FULL_AGGREGATE_OVER {
            return Self::full_aggregate(conn).await;
        }
        let mut depth: HashMap<i64, u32> = HashMap::new();
        for &id in &all {
            let mut d = 0;
            let mut cur = id;
            while cur != 0 {
                if let Some(&known) = depth.get(&cur) {
                    d += known;
                    break;
                }
                d += 1;
                cur = parents.get(&cur).copied().unwrap_or(0);
            }
            depth.insert(id, d);
        }
        let mut order: Vec<(u32, i64)> = all.into_iter().map(|id| (depth[&id], id)).collect();
        order.sort_unstable_by(|a, b| b.cmp(a));
        for (_, id) in order {
            let mut rows = query(
                conn,
                "SELECT e.size, e.alloc, \
                     (SELECT coalesce(sum(t_size), 0) FROM entries WHERE parent = e.id), \
                     (SELECT coalesce(sum(t_alloc), 0) FROM entries WHERE parent = e.id), \
                     (SELECT coalesce(sum(t_files), 0) FROM entries WHERE parent = e.id), \
                     (SELECT coalesce(sum(t_dirs), 0) FROM entries WHERE parent = e.id) \
                     FROM entries e WHERE e.id = ?1",
                (id,),
            )
            .await?;
            let Some(row) = rows.next().await? else {
                continue;
            };
            let vals = (
                int(&row, 0)? + int(&row, 2)?,
                int(&row, 1)? + int(&row, 3)?,
                int(&row, 4)?,
                int(&row, 5)? + 1,
            );
            drop(rows);
            exec(
                conn,
                "UPDATE entries SET t_size=?1, t_alloc=?2, t_files=?3, t_dirs=?4 WHERE id=?5",
                (vals.0, vals.1, vals.2, vals.3, id),
            )
            .await?;
        }
        Ok(())
    }

    /// Recompute every total from the leaves up in one pass over the table,
    /// writing only rows whose stored totals are wrong.
    async fn full_aggregate(conn: &Connection) -> Result<()> {
        struct Dir {
            parent: i64,
            own: [i64; 2],
            stored: [i64; 4],
        }
        let mut dirs: HashMap<i64, Dir> = HashMap::new();
        let mut acc: HashMap<i64, [i64; 4]> = HashMap::new();
        let mut leaf_fixes: Vec<(i64, [i64; 4])> = Vec::new();
        let mut rows = query(
            conn,
            "SELECT id, parent, kind, size, alloc, nlink, t_size, t_alloc, t_files, \
                 t_dirs FROM entries",
            (),
        )
        .await?;
        while let Some(row) = rows.next().await? {
            let (id, parent) = (int(&row, 0)?, int(&row, 1)?);
            let kind = Kind::from_i64(int(&row, 2)?);
            let stored = [int(&row, 6)?, int(&row, 7)?, int(&row, 8)?, int(&row, 9)?];
            if kind == Kind::Dir {
                let own = [int(&row, 3)?, int(&row, 4)?];
                dirs.insert(
                    id,
                    Dir {
                        parent,
                        own,
                        stored,
                    },
                );
                continue;
            }
            let meta = Meta {
                kind,
                size: int(&row, 3)? as u64,
                alloc: int(&row, 4)? as u64,
                nlink: int(&row, 5)? as u64,
                mode: 0,
                uid: 0,
                gid: 0,
                dev: 0,
                ino: 0,
                mtime_ns: 0,
                ctime_ns: 0,
            };
            let (a, b, c, d) = leaf_totals(&meta);
            let t = [a, b, c, d];
            if t != stored {
                leaf_fixes.push((id, t));
            }
            let slot = acc.entry(parent).or_default();
            for i in 0..4 {
                slot[i] += t[i];
            }
        }
        drop(rows);

        let mut depth: HashMap<i64, u32> = HashMap::with_capacity(dirs.len());
        for &id in dirs.keys() {
            let mut chain = Vec::new();
            let mut cur = id;
            let base = loop {
                if let Some(&d) = depth.get(&cur) {
                    break d;
                }
                chain.push(cur);
                match dirs.get(&cur) {
                    Some(d) if d.parent != 0 => cur = d.parent,
                    _ => break 0,
                };
            };
            for (i, c) in chain.into_iter().rev().enumerate() {
                depth.insert(c, base + i as u32 + 1);
            }
        }
        let mut order: Vec<(u32, i64)> = depth.into_iter().map(|(id, d)| (d, id)).collect();
        order.sort_unstable_by(|a, b| b.cmp(a));

        let mut dir_fixes: Vec<(i64, [i64; 4])> = Vec::new();
        for (_, id) in order {
            let d = &dirs[&id];
            let kids = acc.get(&id).copied().unwrap_or_default();
            let t = [d.own[0] + kids[0], d.own[1] + kids[1], kids[2], kids[3] + 1];
            if t != d.stored {
                dir_fixes.push((id, t));
            }
            if d.parent != 0 {
                let slot = acc.entry(d.parent).or_default();
                for i in 0..4 {
                    slot[i] += t[i];
                }
            }
        }
        for (id, t) in leaf_fixes.into_iter().chain(dir_fixes) {
            exec(
                conn,
                "UPDATE entries SET t_size=?1, t_alloc=?2, t_files=?3, t_dirs=?4 WHERE id=?5",
                (t[0], t[1], t[2], t[3], id),
            )
            .await?;
        }
        Ok(())
    }

    /// Drop an indexed root and everything under it.
    pub async fn remove_root(&self, path: &Path) -> Result<u64> {
        let conn = self.connect()?;
        let Some(root) = self
            .roots(&conn)
            .await?
            .into_iter()
            .find(|r| Path::new(&r.name) == path)
        else {
            return Ok(0);
        };
        exec(&conn, "BEGIN", ()).await?;
        let mut stack = vec![root.id];
        let mut n = 0;
        while let Some(cur) = stack.pop() {
            let mut rows = query(
                &conn,
                "SELECT id FROM entries WHERE parent = ?1 AND kind = 1",
                (cur,),
            )
            .await?;
            while let Some(row) = rows.next().await? {
                stack.push(int(&row, 0)?);
            }
            drop(rows);
            exec(
                &conn,
                "DELETE FROM tags WHERE entry IN (SELECT id FROM entries WHERE parent = ?1)",
                (cur,),
            )
            .await?;
            n += conn
                .execute("DELETE FROM entries WHERE parent = ?1", (cur,))
                .await?;
            exec(&conn, "DELETE FROM tags WHERE entry = ?1", (cur,)).await?;
        }
        n += conn
            .execute("DELETE FROM entries WHERE id = ?1", (root.id,))
            .await?;
        exec(&conn, "COMMIT", ()).await?;
        Ok(n)
    }

    pub async fn children_of(&self, conn: &Connection, id: i64) -> Result<Vec<Record>> {
        let mut rows = query(
            conn,
            format!("SELECT {ENTRY_COLS} FROM entries WHERE parent = ?1"),
            (id,),
        )
        .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(record(&row)?);
        }
        Ok(out)
    }

    pub async fn get(&self, conn: &Connection, id: i64) -> Result<Option<Record>> {
        let mut rows = query(
            conn,
            format!("SELECT {ENTRY_COLS} FROM entries WHERE id = ?1"),
            (id,),
        )
        .await?;
        rows.next().await?.map(|r| record(&r)).transpose()
    }

    pub async fn roots(&self, conn: &Connection) -> Result<Vec<Record>> {
        self.children_of(conn, 0).await
    }

    /// The row for an absolute path, if it lies under an indexed root.
    pub async fn resolve(&self, conn: &Connection, path: &Path) -> Result<Option<i64>> {
        let path = std::path::absolute(path)?;
        let best = self
            .roots(conn)
            .await?
            .into_iter()
            .filter(|r| path.starts_with(Path::new(&r.name)))
            .max_by_key(|r| r.name.len());
        let Some(root) = best else { return Ok(None) };
        let mut id = root.id;
        let rest = path.strip_prefix(Path::new(&root.name))?;
        for comp in rest.components() {
            let mut rows = query(
                conn,
                "SELECT id FROM entries WHERE parent = ?1 AND name = ?2",
                (id, name_bytes(comp.as_os_str())),
            )
            .await?;
            match rows.next().await? {
                Some(row) => id = int(&row, 0)?,
                None => return Ok(None),
            }
        }
        Ok(Some(id))
    }

    pub async fn path_of(
        &self,
        conn: &Connection,
        id: i64,
        cache: &mut HashMap<i64, PathBuf>,
    ) -> Result<PathBuf> {
        let mut chain = Vec::new();
        let mut cur = id;
        let base = loop {
            if let Some(p) = cache.get(&cur) {
                break p.clone();
            }
            let mut rows = query(
                conn,
                "SELECT parent, name FROM entries WHERE id = ?1",
                (cur,),
            )
            .await?;
            let row = rows.next().await?.context("dangling entry")?;
            let (parent, name) = (int(&row, 0)?, blob(&row, 1)?);
            chain.push((cur, OsString::from_vec(name)));
            if parent == 0 {
                break PathBuf::new();
            }
            cur = parent;
        };
        let mut path = base;
        for (cid, name) in chain.into_iter().rev() {
            path.push(name);
            cache.insert(cid, path.clone());
        }
        Ok(path)
    }

    /// Entries whose name matches: a glob when `pattern` has `*` or `?`,
    /// otherwise a case-insensitive substring.
    pub async fn locate(
        &self,
        conn: &Connection,
        pattern: &str,
        limit: u32,
    ) -> Result<Vec<(i64, PathBuf)>> {
        let (sql, pat) = if pattern.contains(['*', '?', '[']) {
            (
                "SELECT id FROM entries WHERE name GLOB ?1 LIMIT ?2",
                pattern.to_string(),
            )
        } else {
            (
                "SELECT id FROM entries WHERE name LIKE ?1 LIMIT ?2",
                format!("%{pattern}%"),
            )
        };
        let mut rows = query(conn, sql, (pat, i64::from(limit))).await?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next().await? {
            ids.push(int(&row, 0)?);
        }
        drop(rows);
        let mut cache = HashMap::new();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push((id, self.path_of(conn, id, &mut cache).await?));
        }
        Ok(out)
    }

    /// Every entry under `id` (inclusive) with its path.
    pub async fn subtree(
        &self,
        conn: &Connection,
        id: i64,
        base: &Path,
    ) -> Result<Vec<(Record, PathBuf)>> {
        let Some(top) = self.get(conn, id).await? else {
            return Ok(vec![]);
        };
        let mut out = vec![(top, base.to_path_buf())];
        let mut i = 0;
        while i < out.len() {
            if out[i].0.meta.kind == Kind::Dir {
                let (pid, ppath) = (out[i].0.id, out[i].1.clone());
                for r in self.children_of(conn, pid).await? {
                    let p = ppath.join(&r.name);
                    out.push((r, p));
                }
            }
            i += 1;
        }
        Ok(out)
    }

    /// Whether a subtree of `dirs` directories is a big enough share of the
    /// index that one pass over the table beats a query per directory.
    async fn mostly_everything(&self, conn: &Connection, dirs: u64) -> Result<bool> {
        let mut rows = query(conn, "SELECT sum(t_dirs) FROM entries WHERE parent = 0", ()).await?;
        let total = match rows.next().await? {
            Some(row) => int(&row, 0)? as u64,
            None => 0,
        };
        Ok(dirs.saturating_mul(4) > total)
    }

    /// The directories under `id` (inclusive, `id` first), plus any other
    /// entries named in `files`: the shape of a tree without holding every
    /// entry. Use `children_nodes` for a directory's full contents.
    pub async fn nodes_under(
        &self,
        conn: &Connection,
        id: i64,
        files: &[&str],
    ) -> Result<Vec<Node>> {
        let Some(top) = self.get(conn, id).await? else {
            return Ok(vec![]);
        };
        let mut out = vec![Node {
            id: top.id,
            parent: top.parent,
            name: top.name.as_bytes().into(),
            is_dir: top.meta.kind == Kind::Dir,
        }];
        if top.meta.kind != Kind::Dir {
            return Ok(out);
        }
        let names = files.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let wanted = |params: &mut Vec<Value>| {
            params.extend(files.iter().map(|f| Value::Blob(f.as_bytes().to_vec())));
        };
        let node = |row: &Row| -> Result<Node> {
            Ok(Node {
                id: int(row, 0)?,
                parent: int(row, 1)?,
                name: blob(row, 2)?.into(),
                is_dir: Kind::from_i64(int(row, 3)?) == Kind::Dir,
            })
        };
        if self.mostly_everything(conn, top.t_dirs).await? {
            let mut kids: HashMap<i64, Vec<Node>> = HashMap::new();
            let mut params = Vec::new();
            wanted(&mut params);
            let mut rows = query(
                conn,
                format!(
                    "SELECT id, parent, name, kind FROM entries WHERE kind = 1 OR name IN ({names})"
                ),
                params,
            )
            .await?;
            while let Some(row) = rows.next().await? {
                let n = node(&row)?;
                kids.entry(n.parent).or_default().push(n);
            }
            let mut i = 0;
            while i < out.len() {
                if out[i].is_dir
                    && let Some(children) = kids.remove(&out[i].id)
                {
                    out.extend(children);
                }
                i += 1;
            }
        } else {
            let sql = format!(
                "SELECT id, parent, name, kind FROM entries \
                 WHERE parent = ?1 AND (kind = 1 OR name IN ({}))",
                (0..files.len())
                    .map(|i| format!("?{}", i + 2))
                    .collect::<Vec<_>>()
                    .join(",")
            );
            let mut i = 0;
            while i < out.len() {
                if out[i].is_dir {
                    let mut params = vec![Value::Integer(out[i].id)];
                    wanted(&mut params);
                    let mut rows = query(conn, &sql, params).await?;
                    while let Some(row) = rows.next().await? {
                        out.push(node(&row)?);
                    }
                }
                i += 1;
            }
        }
        Ok(out)
    }

    /// Every entry directly in directory `id`, without metadata.
    pub async fn children_nodes(&self, conn: &Connection, id: i64) -> Result<Vec<Node>> {
        let mut rows = query(
            conn,
            "SELECT id, parent, name, kind FROM entries WHERE parent = ?1",
            (id,),
        )
        .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(Node {
                id: int(&row, 0)?,
                parent: int(&row, 1)?,
                name: blob(&row, 2)?.into(),
                is_dir: Kind::from_i64(int(&row, 3)?) == Kind::Dir,
            });
        }
        Ok(out)
    }

    pub async fn tags_of(&self, conn: &Connection, id: i64) -> Result<Vec<String>> {
        let mut rows = query(conn, "SELECT source, tag FROM tags WHERE entry = ?1", (id,)).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let (s, t) = (blob(&row, 0)?, blob(&row, 1)?);
            out.push(format!(
                "{}:{}",
                String::from_utf8_lossy(&s),
                String::from_utf8_lossy(&t)
            ));
        }
        Ok(out)
    }

    /// Tags on `id` and on each of its ancestors, nearest first.
    pub async fn effective_tags(&self, conn: &Connection, id: i64) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut cur = id;
        while cur != 0 {
            out.extend(self.tags_of(conn, cur).await?);
            let Some(r) = self.get(conn, cur).await? else {
                break;
            };
            cur = r.parent;
        }
        Ok(out)
    }

    /// Replace every tag `source` owns on the entries `in_scope(id, parent)`
    /// accepts.
    pub async fn replace_tags(
        &self,
        conn: &Connection,
        source: &str,
        in_scope: impl Fn(i64, i64) -> bool,
        tags: &[(i64, String)],
    ) -> Result<()> {
        let mut rows = query(
            conn,
            "SELECT DISTINCT t.entry, e.parent FROM tags t LEFT JOIN entries e ON e.id = t.entry \
             WHERE t.source = ?1",
            (source,),
        )
        .await?;
        let mut stale = Vec::new();
        while let Some(row) = rows.next().await? {
            let id = int(&row, 0)?;
            if in_scope(id, int(&row, 1)?) {
                stale.push(id);
            }
        }
        drop(rows);
        exec(conn, "BEGIN", ()).await?;
        for id in stale {
            exec(
                conn,
                "DELETE FROM tags WHERE entry = ?1 AND source = ?2",
                (id, source),
            )
            .await?;
        }
        for (id, tag) in tags {
            exec(
                conn,
                "INSERT OR IGNORE INTO tags (entry, source, tag) VALUES (?1, ?2, ?3)",
                (*id, source, tag.as_str()),
            )
            .await?;
        }
        exec(conn, "COMMIT", ()).await?;
        Ok(())
    }

    pub async fn content_id(
        &self,
        conn: &Connection,
        dev: u64,
        ino: u64,
    ) -> Result<Option<StoredContentId>> {
        let mut rows = query(
            conn,
            "SELECT size, mtime_ns, ctime_ns, root FROM contentids \
                 WHERE dev = ?1 AND ino = ?2",
            (dev as i64, ino as i64),
        )
        .await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        Ok(Some(StoredContentId {
            size: int(&row, 0)? as u64,
            mtime_ns: int(&row, 1)?,
            ctime_ns: int(&row, 2)?,
            root: blob(&row, 3)?,
        }))
    }

    /// A stored content id is only valid while the file's stat fields match
    /// the ones it was hashed under.
    pub async fn fresh_content_id(&self, conn: &Connection, m: &Meta) -> Result<Option<Vec<u8>>> {
        Ok(self.content_id(conn, m.dev, m.ino).await?.and_then(|c| {
            (c.size == m.size && c.mtime_ns == m.mtime_ns && c.ctime_ns == m.ctime_ns)
                .then_some(c.root)
        }))
    }

    pub async fn put_content_id(&self, conn: &Connection, m: &Meta, root: &[u8]) -> Result<()> {
        exec(
            conn,
            "INSERT OR REPLACE INTO contentids (dev, ino, size, mtime_ns, ctime_ns, root) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (
                m.dev as i64,
                m.ino as i64,
                m.size as i64,
                m.mtime_ns,
                m.ctime_ns,
                root.to_vec(),
            ),
        )
        .await?;
        Ok(())
    }

    /// Content ids stored for more than one inode, with their size.
    pub async fn shared_content_ids(&self, conn: &Connection) -> Result<Vec<(Vec<u8>, u64)>> {
        let mut rows = query(
            conn,
            "SELECT root, max(size) FROM contentids GROUP BY root HAVING count(*) > 1",
            (),
        )
        .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push((blob(&row, 0)?, int(&row, 1)? as u64));
        }
        Ok(out)
    }

    /// Indexed entries whose current stat still matches content `root`.
    pub async fn find_content(&self, conn: &Connection, root: &[u8]) -> Result<Vec<i64>> {
        let mut rows = query(
            conn,
            "SELECT e.id FROM contentids c JOIN entries e \
                 ON e.dev = c.dev AND e.ino = c.ino AND e.size = c.size \
                 AND e.mtime_ns = c.mtime_ns AND e.ctime_ns = c.ctime_ns \
                 WHERE c.root = ?1",
            (root.to_vec(),),
        )
        .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(int(&row, 0)?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
