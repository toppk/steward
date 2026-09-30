//! Parallel walk producing one [`Listing`] per directory.
//!
//! A directory's listing is always sent before any of its subdirectories are
//! walked, so the single writer on the other end of the channel has written
//! a directory's row by the time that directory's own listing arrives.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, Metadata};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use tokio::sync::mpsc::Sender;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    File = 0,
    Dir = 1,
    Symlink = 2,
    Other = 3,
    BlockDev = 4,
    CharDev = 5,
    Fifo = 6,
    Socket = 7,
}

impl Kind {
    pub const fn from_i64(v: i64) -> Self {
        match v {
            0 => Self::File,
            1 => Self::Dir,
            2 => Self::Symlink,
            4 => Self::BlockDev,
            5 => Self::CharDev,
            6 => Self::Fifo,
            7 => Self::Socket,
            _ => Self::Other,
        }
    }
}

/// The subset of `lstat` the index keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    pub kind: Kind,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub alloc: u64,
    pub nlink: u64,
    pub dev: u64,
    pub ino: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

impl Meta {
    pub fn from_std(m: &Metadata) -> Self {
        let ft = m.file_type();
        let kind = if ft.is_dir() {
            Kind::Dir
        } else if ft.is_file() {
            Kind::File
        } else if ft.is_symlink() {
            Kind::Symlink
        } else if ft.is_block_device() {
            Kind::BlockDev
        } else if ft.is_char_device() {
            Kind::CharDev
        } else if ft.is_fifo() {
            Kind::Fifo
        } else if ft.is_socket() {
            Kind::Socket
        } else {
            Kind::Other
        };
        Self {
            kind,
            mode: m.mode() & 0o7777,
            uid: m.uid(),
            gid: m.gid(),
            size: m.size(),
            alloc: m.blocks() * 512,
            nlink: m.nlink(),
            dev: m.dev(),
            ino: m.ino(),
            mtime_ns: m.mtime() * 1_000_000_000 + m.mtime_nsec(),
            // Only directories' ctime is used (trusting rescans); a file's
            // changes on rename, chmod or a new hard link, none of which the
            // index needs to notice, and it would cost 8 bytes per row.
            ctime_ns: if kind == Kind::Dir {
                m.ctime() * 1_000_000_000 + m.ctime_nsec()
            } else {
                0
            },
        }
    }

    pub fn lstat(path: &Path) -> std::io::Result<Self> {
        fs::symlink_metadata(path).map(|m| Self::from_std(&m))
    }
}

/// One directory as the walk found it. Directory ids travel with the walk:
/// known directories keep their row id, new ones get a fresh id here, so the
/// writer never has to map paths back to rows.
#[derive(Debug)]
pub struct Listing {
    pub id: i64,
    pub parent: i64,
    pub path: PathBuf,
    pub meta: Meta,
    /// The index held this directory when the scan started.
    pub known: bool,
    /// `None` when the directory was trusted unchanged or could not be read;
    /// the writer then leaves its stored children alone.
    pub entries: Option<Vec<Child>>,
}

#[derive(Debug)]
pub struct Child {
    pub name: OsString,
    pub meta: Meta,
    /// Every directory child has an id, whether or not the walk descends.
    pub dir_id: Option<i64>,
}

/// What the index held for a directory when the scan started: its parent,
/// times and subdirectories, by id and name, never full paths (the ncdu and
/// qdirstat model).
#[derive(Debug)]
pub struct KnownDir {
    pub parent: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub subdirs: Vec<(Box<[u8]>, i64)>,
}

pub type Snapshot = HashMap<i64, KnownDir>;

#[derive(Debug, Default)]
pub struct Counters {
    pub dirs_read: AtomicU64,
    pub dirs_trusted: AtomicU64,
    pub entries: AtomicU64,
    pub errors: AtomicU64,
    pub cancelled: AtomicBool,
}

pub struct Walker {
    pub exclude: Option<ignore::gitignore::Gitignore>,
    pub snapshot: Arc<Snapshot>,
    /// Shared with the writer, which numbers new non-directory rows.
    pub next_id: Arc<AtomicI64>,
    /// Skip `readdir` of a directory whose mtime and ctime match the index,
    /// the updatedb trick: names only change when the directory's mtime does.
    /// File size changes inside such a directory are missed until a full scan.
    pub trust_dir_mtime: bool,
    pub one_filesystem: bool,
    pub counters: Arc<Counters>,
    pub tx: Sender<Listing>,
}

impl Walker {
    /// Blocks until the whole tree under `root` (row `id`) has been sent.
    pub fn run(&self, root: &Path, id: i64, parent: i64) {
        let meta = match Meta::lstat(root) {
            Ok(m) if m.kind == Kind::Dir => m,
            Ok(_) | Err(_) => {
                self.counters.errors.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        rayon::scope(|s| self.visit(s, root.to_path_buf(), id, parent, meta));
    }

    fn visit<'s>(&'s self, s: &rayon::Scope<'s>, path: PathBuf, id: i64, parent: i64, meta: Meta) {
        if self.counters.cancelled.load(Ordering::Relaxed) {
            return;
        }
        let known = self.snapshot.get(&id);
        let trusted = self.trust_dir_mtime
            && known.is_some_and(|k| k.mtime_ns == meta.mtime_ns && k.ctime_ns == meta.ctime_ns);

        let mut subdirs = Vec::new();
        let entries = if let (true, Some(k)) = (trusted, known) {
            self.counters.dirs_trusted.fetch_add(1, Ordering::Relaxed);
            for (name, cid) in &k.subdirs {
                let child = path.join(OsStr::from_bytes(name));
                if let Ok(m) = Meta::lstat(&child)
                    && m.kind == Kind::Dir
                    && !self.excluded(&child, true)
                    && (!self.one_filesystem || m.dev == meta.dev)
                {
                    subdirs.push((child, *cid, m));
                }
            }
            None
        } else {
            self.read_dir(&path, known, meta.dev, &mut subdirs)
        };

        let listing = Listing {
            id,
            parent,
            path,
            meta,
            known: known.is_some(),
            entries,
        };
        if self.tx.blocking_send(listing).is_err() {
            self.counters.cancelled.store(true, Ordering::Relaxed);
            return;
        }
        for (child, cid, m) in subdirs {
            s.spawn(move |s| self.visit(s, child, cid, id, m));
        }
    }

    fn excluded(&self, path: &Path, is_dir: bool) -> bool {
        self.exclude
            .as_ref()
            .is_some_and(|g| g.matched(path, is_dir).is_ignore())
    }

    fn read_dir(
        &self,
        path: &Path,
        known: Option<&KnownDir>,
        dev: u64,
        subdirs: &mut Vec<(PathBuf, i64, Meta)>,
    ) -> Option<Vec<Child>> {
        let rd = match fs::read_dir(path) {
            Ok(rd) => rd,
            Err(_) => {
                self.counters.errors.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        self.counters.dirs_read.fetch_add(1, Ordering::Relaxed);
        let ids: HashMap<&[u8], i64> = known
            .map(|k| k.subdirs.iter().map(|(n, i)| (&n[..], *i)).collect())
            .unwrap_or_default();
        let mut out = Vec::new();
        for entry in rd {
            let Ok(entry) = entry else {
                self.counters.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            let child = entry.path();
            // Raced with a delete; the next scan settles it.
            let Ok(m) = Meta::lstat(&child) else {
                continue;
            };
            if self.excluded(&child, m.kind == Kind::Dir) {
                continue;
            }
            let name = entry.file_name();
            let dir_id = (m.kind == Kind::Dir).then(|| {
                ids.get(name.as_bytes())
                    .copied()
                    .unwrap_or_else(|| self.next_id.fetch_add(1, Ordering::Relaxed))
            });
            if let Some(cid) = dir_id
                && (!self.one_filesystem || m.dev == dev)
            {
                subdirs.push((child, cid, m));
            }
            out.push(Child {
                name,
                meta: m,
                dir_id,
            });
        }
        self.counters
            .entries
            .fetch_add(out.len() as u64, Ordering::Relaxed);
        Some(out)
    }
}
