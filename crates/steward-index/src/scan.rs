//! Parallel walk producing one [`Listing`] per directory.
//!
//! A directory's listing is always sent before any of its subdirectories are
//! walked, so the single writer on the other end of the channel always knows
//! the parent's row id by the time a child listing arrives.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::sync::mpsc::Sender;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    File = 0,
    Dir = 1,
    Symlink = 2,
    Other = 3,
}

impl Kind {
    pub const fn from_i64(v: i64) -> Self {
        match v {
            0 => Self::File,
            1 => Self::Dir,
            2 => Self::Symlink,
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
            ctime_ns: m.ctime() * 1_000_000_000 + m.ctime_nsec(),
        }
    }

    pub fn lstat(path: &Path) -> std::io::Result<Self> {
        fs::symlink_metadata(path).map(|m| Self::from_std(&m))
    }
}

#[derive(Debug)]
pub struct Listing {
    pub path: PathBuf,
    pub meta: Meta,
    /// `None` when the directory was trusted unchanged or could not be read;
    /// the writer then leaves its stored children alone.
    pub entries: Option<Vec<(OsString, Meta)>>,
}

/// What the index already knows about a directory.
#[derive(Clone, Debug)]
pub struct DirSnap {
    pub id: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub subdirs: Vec<OsString>,
}

#[derive(Debug, Default)]
pub struct Counters {
    pub dirs_read: AtomicU64,
    pub dirs_trusted: AtomicU64,
    pub entries: AtomicU64,
    pub errors: AtomicU64,
    pub cancelled: AtomicBool,
}

pub struct Walker {
    pub snapshot: Arc<HashMap<PathBuf, DirSnap>>,
    /// Skip `readdir` of a directory whose mtime and ctime match the index,
    /// the updatedb trick: names only change when the directory's mtime does.
    /// File size changes inside such a directory are missed until a full scan.
    pub trust_dir_mtime: bool,
    pub one_filesystem: bool,
    pub counters: Arc<Counters>,
    pub tx: Sender<Listing>,
}

impl Walker {
    /// Blocks until the whole tree under `root` has been sent.
    pub fn run(&self, root: &Path) {
        let meta = match Meta::lstat(root) {
            Ok(m) if m.kind == Kind::Dir => m,
            Ok(_) | Err(_) => {
                self.counters.errors.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        rayon::scope(|s| self.visit(s, root.to_path_buf(), meta));
    }

    fn visit<'s>(&'s self, s: &rayon::Scope<'s>, path: PathBuf, meta: Meta) {
        if self.counters.cancelled.load(Ordering::Relaxed) {
            return;
        }
        let known = self.snapshot.get(&path);
        let trusted = self.trust_dir_mtime
            && known.is_some_and(|k| k.mtime_ns == meta.mtime_ns && k.ctime_ns == meta.ctime_ns);

        let mut subdirs = Vec::new();
        let entries = if trusted {
            self.counters.dirs_trusted.fetch_add(1, Ordering::Relaxed);
            for name in &known.expect("trusted implies known").subdirs {
                let child = path.join(name);
                if let Ok(m) = Meta::lstat(&child)
                    && m.kind == Kind::Dir
                    && (!self.one_filesystem || m.dev == meta.dev)
                {
                    subdirs.push((child, m));
                }
            }
            None
        } else {
            self.read_dir(&path, meta.dev, &mut subdirs)
        };

        let listing = Listing {
            path,
            meta,
            entries,
        };
        if self.tx.blocking_send(listing).is_err() {
            self.counters.cancelled.store(true, Ordering::Relaxed);
            return;
        }
        for (child, m) in subdirs {
            s.spawn(move |s| self.visit(s, child, m));
        }
    }

    fn read_dir(
        &self,
        path: &Path,
        dev: u64,
        subdirs: &mut Vec<(PathBuf, Meta)>,
    ) -> Option<Vec<(OsString, Meta)>> {
        let rd = match fs::read_dir(path) {
            Ok(rd) => rd,
            Err(_) => {
                self.counters.errors.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        self.counters.dirs_read.fetch_add(1, Ordering::Relaxed);
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
            if m.kind == Kind::Dir && (!self.one_filesystem || m.dev == dev) {
                subdirs.push((child, m));
            }
            out.push((entry.file_name(), m));
        }
        self.counters
            .entries
            .fetch_add(out.len() as u64, Ordering::Relaxed);
        Some(out)
    }
}
