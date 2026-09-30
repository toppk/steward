//! Layer 1: the filesystem index. One row per path with its `lstat` fields,
//! plus subtree totals on directories, kept current by rescans that only
//! write what changed.

pub mod scan;

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use turso::{Connection, Database, Row, Value};

use scan::{Counters, DirSnap, Listing, Walker};
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
";

const ENTRY_COLS: &str = "id, parent, name, kind, mode, uid, gid, size, \
    alloc, nlink, dev, ino, mtime_ns, ctime_ns, t_size, t_alloc, t_files, t_dirs";

/// Writes per transaction during a scan, so readers see progress and a
/// crash loses little.
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

#[derive(Clone, Copy, Debug)]
pub struct ScanOptions {
    pub trust_dir_mtime: bool,
    pub one_filesystem: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            trust_dir_mtime: false,
            one_filesystem: true,
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
    ((m.size / n) as i64, (m.alloc / n) as i64, 1, 0)
}

async fn insert(conn: &Connection, parent: i64, name: Vec<u8>, m: &Meta) -> Result<i64> {
    let (ts, ta, tf, td) = if m.kind == Kind::Dir {
        (0, 0, 0, 1)
    } else {
        leaf_totals(m)
    };
    conn.execute(
        "INSERT INTO entries (parent, name, kind, mode, uid, gid, size, alloc, \
         nlink, dev, ino, mtime_ns, ctime_ns, t_size, t_alloc, t_files, t_dirs) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, \
         ?15, ?16, ?17)",
        vec![
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
        ],
    )
    .await?;
    Ok(conn.last_insert_rowid())
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
    conn.execute(sql, params).await?;
    Ok(())
}

/// Scan-time bookkeeping for directories: what the index had, plus what
/// this scan inserted.
#[derive(Default)]
struct DirMap {
    ids: HashMap<PathBuf, i64>,
    paths: HashMap<i64, PathBuf>,
    parents: HashMap<i64, i64>,
}

impl DirMap {
    fn add(&mut self, id: i64, parent: i64, path: PathBuf) {
        self.ids.insert(path.clone(), id);
        self.paths.insert(id, path);
        self.parents.insert(id, parent);
    }

    fn remove(&mut self, id: i64) {
        if let Some(p) = self.paths.remove(&id) {
            self.ids.remove(&p);
        }
        self.parents.remove(&id);
    }
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
        Ok(Self { db })
    }

    pub fn connect(&self) -> Result<Connection> {
        Ok(self.db.connect()?)
    }

    async fn load_dirs(conn: &Connection) -> Result<(DirMap, HashMap<PathBuf, DirSnap>)> {
        let mut rows = conn
            .query(
                "SELECT id, parent, name, mtime_ns, ctime_ns FROM entries \
                 WHERE kind = 1",
                (),
            )
            .await?;
        let mut raw: HashMap<i64, (i64, Vec<u8>, i64, i64)> = HashMap::new();
        while let Some(row) = rows.next().await? {
            raw.insert(
                int(&row, 0)?,
                (int(&row, 1)?, blob(&row, 2)?, int(&row, 3)?, int(&row, 4)?),
            );
        }
        let mut map = DirMap::default();
        for &id in raw.keys() {
            resolve_path(id, &raw, &mut map);
        }
        let mut snap: HashMap<PathBuf, DirSnap> = HashMap::new();
        for (id, (_, _, mt, ct)) in &raw {
            snap.insert(
                map.paths[id].clone(),
                DirSnap {
                    id: *id,
                    mtime_ns: *mt,
                    ctime_ns: *ct,
                    subdirs: vec![],
                },
            );
        }
        for (id, (parent, name, _, _)) in &raw {
            if let Some(pp) = map.paths.get(parent) {
                let _ = id;
                if let Some(s) = snap.get_mut(pp) {
                    s.subdirs.push(OsString::from_vec(name.clone()));
                }
            }
        }
        Ok((map, snap))
    }

    /// Bring the index under `root` up to date with the filesystem.
    pub async fn scan(&self, root: &Path, opts: ScanOptions) -> Result<ScanStats> {
        let started = Instant::now();
        let root =
            std::fs::canonicalize(root).with_context(|| format!("resolving {}", root.display()))?;
        let conn = self.connect()?;
        let (mut dirs, snapshot) = Self::load_dirs(&conn).await?;
        let snapshot = Arc::new(snapshot);

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Listing>(4096);
        let counters = Arc::new(Counters::default());
        let walker = Walker {
            snapshot: Arc::clone(&snapshot),
            trust_dir_mtime: opts.trust_dir_mtime,
            one_filesystem: opts.one_filesystem,
            counters: Arc::clone(&counters),
            tx,
        };
        let walk_root = root.clone();
        let walk = tokio::task::spawn_blocking(move || walker.run(&walk_root));

        let mut stats = ScanStats {
            root: root.clone(),
            ..ScanStats::default()
        };
        let mut dirty: HashSet<i64> = HashSet::new();
        let mut pending: u64 = 0;
        conn.execute("BEGIN", ()).await?;
        let result: Result<()> = async {
            while let Some(listing) = rx.recv().await {
                let before = stats.inserted + stats.updated + stats.deleted;
                self.apply(
                    &conn, &root, &listing, &mut dirs, &snapshot, &mut dirty, &mut stats,
                )
                .await?;
                pending += stats.inserted + stats.updated + stats.deleted - before;
                if pending >= COMMIT_EVERY {
                    conn.execute("COMMIT", ()).await?;
                    conn.execute("BEGIN", ()).await?;
                    pending = 0;
                }
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            counters.cancelled.store(true, Ordering::Relaxed);
            drop(rx);
            let _ = walk.await;
            let _ = conn.execute("ROLLBACK", ()).await;
            return Err(e);
        }
        walk.await?;

        if let Some(&root_id) = dirs.ids.get(&root) {
            let mut up = dirs.parents.get(&root_id).copied();
            while let Some(p) = up.filter(|&p| p != 0) {
                dirty.insert(p);
                up = dirs.parents.get(&p).copied();
            }
        }
        Self::aggregate(&conn, &dirs, dirty).await?;
        conn.execute("COMMIT", ()).await?;

        stats.dirs_read = counters.dirs_read.load(Ordering::Relaxed);
        stats.dirs_trusted = counters.dirs_trusted.load(Ordering::Relaxed);
        stats.entries_seen = counters.entries.load(Ordering::Relaxed);
        stats.errors = counters.errors.load(Ordering::Relaxed);
        stats.millis = started.elapsed().as_millis() as u64;
        Ok(stats)
    }

    #[allow(clippy::too_many_arguments)]
    async fn apply(
        &self,
        conn: &Connection,
        root: &Path,
        listing: &Listing,
        dirs: &mut DirMap,
        snapshot: &HashMap<PathBuf, DirSnap>,
        dirty: &mut HashSet<i64>,
        stats: &mut ScanStats,
    ) -> Result<()> {
        let id = match dirs.ids.get(&listing.path) {
            Some(&id) => {
                // Not in the snapshot means inserted by this scan from the
                // parent's listing, already with this metadata.
                let changed = snapshot.get(&listing.path).is_some_and(|s| {
                    s.mtime_ns != listing.meta.mtime_ns || s.ctime_ns != listing.meta.ctime_ns
                });
                if changed {
                    update_meta(conn, id, &listing.meta).await?;
                    stats.updated += 1;
                }
                id
            }
            None if listing.path == root => {
                let (parent, name) = match root.parent().and_then(|p| dirs.ids.get(p)) {
                    Some(&pid) => (pid, name_bytes(root.file_name().unwrap_or_default())),
                    None => (0, name_bytes(root.as_os_str())),
                };
                let id = insert(conn, parent, name, &listing.meta).await?;
                stats.inserted += 1;
                dirs.add(id, parent, root.to_path_buf());
                dirty.insert(id);
                id
            }
            None => bail!("listing for unknown directory {}", listing.path.display()),
        };
        let Some(entries) = &listing.entries else {
            return Ok(());
        };

        let mut stored: HashMap<Vec<u8>, Record> = HashMap::new();
        for r in self.children_of(conn, id).await? {
            stored.insert(name_bytes(&r.name), r);
        }
        let mut changed = false;
        for (name, m) in entries {
            let key = name_bytes(name);
            match stored.remove(&key) {
                Some(old) if old.meta.kind == m.kind => {
                    // A walked subdirectory updates its own row with its
                    // listing, so a stored mtime never runs ahead of the
                    // stored children and fools a later trusting scan.
                    let own_listing = m.kind == Kind::Dir && m.dev == listing.meta.dev;
                    if old.meta != *m && !own_listing {
                        update_meta(conn, old.id, m).await?;
                        stats.updated += 1;
                        changed = true;
                    }
                }
                other => {
                    if let Some(old) = other {
                        stats.deleted += self.delete_subtree(conn, old.id, dirs).await?;
                    }
                    let path = listing.path.join(name);
                    let adopted = match dirs.ids.get(&path) {
                        // A former root now reached from above: re-parent it.
                        Some(&rid) if dirs.parents.get(&rid) == Some(&0) => {
                            conn.execute(
                                "UPDATE entries SET parent=?1, name=?2 WHERE id=?3",
                                (id, key.clone(), rid),
                            )
                            .await?;
                            dirs.parents.insert(rid, id);
                            Some(rid)
                        }
                        _ => None,
                    };
                    let cid = match adopted {
                        Some(rid) => rid,
                        None => insert(conn, id, key, m).await?,
                    };
                    stats.inserted += 1;
                    changed = true;
                    if m.kind == Kind::Dir {
                        dirs.add(cid, id, path);
                        dirty.insert(cid);
                    }
                }
            }
        }
        for old in stored.into_values() {
            stats.deleted += self.delete_subtree(conn, old.id, dirs).await?;
            changed = true;
        }
        if changed {
            dirty.insert(id);
        }
        Ok(())
    }

    async fn delete_subtree(&self, conn: &Connection, id: i64, dirs: &mut DirMap) -> Result<u64> {
        let mut stack = vec![id];
        let mut n = 0;
        while let Some(cur) = stack.pop() {
            let mut rows = conn
                .query("SELECT id FROM entries WHERE parent = ?1", (cur,))
                .await?;
            while let Some(row) = rows.next().await? {
                stack.push(int(&row, 0)?);
            }
            conn.execute("DELETE FROM entries WHERE id = ?1", (cur,))
                .await?;
            conn.execute("DELETE FROM tags WHERE entry = ?1", (cur,))
                .await?;
            dirs.remove(cur);
            n += 1;
        }
        Ok(n)
    }

    /// Recompute totals of `dirty` directories and all their ancestors,
    /// deepest first so each sums already-correct children.
    async fn aggregate(conn: &Connection, dirs: &DirMap, dirty: HashSet<i64>) -> Result<()> {
        let mut all: HashSet<i64> = HashSet::new();
        for id in dirty {
            let mut cur = Some(id);
            while let Some(c) = cur.filter(|&c| c != 0) {
                if !all.insert(c) {
                    break;
                }
                cur = dirs.parents.get(&c).copied();
            }
        }
        let mut order: Vec<(usize, i64)> = all
            .into_iter()
            .filter_map(|id| dirs.paths.get(&id).map(|p| (p.components().count(), id)))
            .collect();
        order.sort_unstable_by(|a, b| b.cmp(a));
        for (_, id) in order {
            let mut rows = conn
                .query(
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
            conn.execute(
                "UPDATE entries SET t_size=?1, t_alloc=?2, t_files=?3, t_dirs=?4 \
                 WHERE id=?5",
                (vals.0, vals.1, vals.2, vals.3, id),
            )
            .await?;
        }
        Ok(())
    }

    pub async fn children_of(&self, conn: &Connection, id: i64) -> Result<Vec<Record>> {
        let mut rows = conn
            .query(
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
        let mut rows = conn
            .query(
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
            let mut rows = conn
                .query(
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
            let mut rows = conn
                .query("SELECT parent, name FROM entries WHERE id = ?1", (cur,))
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
        let mut rows = conn.query(sql, (pat, i64::from(limit))).await?;
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

    pub async fn tags_of(&self, conn: &Connection, id: i64) -> Result<Vec<String>> {
        let mut rows = conn
            .query("SELECT source, tag FROM tags WHERE entry = ?1", (id,))
            .await?;
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

    /// Replace every tag `source` owns on the entries in `scope`.
    pub async fn replace_tags(
        &self,
        conn: &Connection,
        source: &str,
        scope: &[i64],
        tags: &[(i64, String)],
    ) -> Result<()> {
        let scope: HashSet<i64> = scope.iter().copied().collect();
        let mut rows = conn
            .query(
                "SELECT DISTINCT entry FROM tags WHERE source = ?1",
                (source,),
            )
            .await?;
        let mut stale = Vec::new();
        while let Some(row) = rows.next().await? {
            let id = int(&row, 0)?;
            if scope.contains(&id) {
                stale.push(id);
            }
        }
        drop(rows);
        conn.execute("BEGIN", ()).await?;
        for id in stale {
            conn.execute(
                "DELETE FROM tags WHERE entry = ?1 AND source = ?2",
                (id, source),
            )
            .await?;
        }
        for (id, tag) in tags {
            conn.execute(
                "INSERT OR IGNORE INTO tags (entry, source, tag) VALUES (?1, ?2, ?3)",
                (*id, source, tag.as_str()),
            )
            .await?;
        }
        conn.execute("COMMIT", ()).await?;
        Ok(())
    }

    pub async fn content_id(
        &self,
        conn: &Connection,
        dev: u64,
        ino: u64,
    ) -> Result<Option<StoredContentId>> {
        let mut rows = conn
            .query(
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
        conn.execute(
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

    /// Indexed entries whose current stat still matches content `root`.
    pub async fn find_content(&self, conn: &Connection, root: &[u8]) -> Result<Vec<i64>> {
        let mut rows = conn
            .query(
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

fn resolve_path(
    id: i64,
    raw: &HashMap<i64, (i64, Vec<u8>, i64, i64)>,
    map: &mut DirMap,
) -> Option<PathBuf> {
    if let Some(p) = map.paths.get(&id) {
        return Some(p.clone());
    }
    let (parent, name, _, _) = raw.get(&id)?;
    let path = if *parent == 0 {
        PathBuf::from(OsStr::from_bytes(name))
    } else {
        resolve_path(*parent, raw, map)?.join(OsStr::from_bytes(name))
    };
    map.add(id, *parent, path.clone());
    Some(path)
}

#[cfg(test)]
mod tests;
