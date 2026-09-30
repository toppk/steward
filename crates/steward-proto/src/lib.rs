//! Wire protocol between `stewardd` and its clients: one JSON object per line
//! over a unix stream socket, a response line for every request line.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub fn socket_path() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(
        || std::env::temp_dir().join(format!("steward-{}", uid())),
        PathBuf::from,
    );
    runtime.join("steward").join("service.socket")
}

fn uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").map_or(0, |m| m.uid())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Status,
    /// Re-read settings.toml: start new roots, prune removed ones, rescan
    /// roots whose settings changed.
    Reload,
    /// Rescan `path` (a configured root, or anything below one).
    Scan {
        path: PathBuf,
        #[serde(default)]
        trust_dir_mtime: bool,
    },
    /// An application changed something under `path`; rescan it soon.
    Invalidate {
        path: PathBuf,
    },
    Stat {
        path: PathBuf,
    },
    Children {
        path: PathBuf,
    },
    Locate {
        pattern: String,
        #[serde(default = "default_limit")]
        limit: u32,
    },
    Classify {
        path: PathBuf,
    },
    ContentId {
        path: PathBuf,
    },
    /// Hash every file under `path` whose content id is missing or stale.
    HashTree {
        path: PathBuf,
    },
    FindContent {
        id: String,
    },
    /// Write the subtree as a qdirstat 2.0 cache file at `out`.
    ExportQdirstat {
        path: PathBuf,
        out: PathBuf,
    },
    /// Settings file location, every root's policy and its index state.
    Settings,
    /// Add a root, or replace the one with the same path; written to
    /// settings.toml (comments kept) and applied.
    PutRoot {
        root: RootSettings,
    },
    /// Stop indexing a root and drop it from the index.
    RemoveRoot {
        path: PathBuf,
    },
    /// Content-id coverage and duplication under `path`.
    ContentSummary {
        path: PathBuf,
    },
    Duplicates {
        path: PathBuf,
        #[serde(default = "default_limit")]
        limit: u32,
    },
}

const fn default_limit() -> u32 {
    1000
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Ok { result: serde_json::Value },
    Error { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub path: String,
    pub kind: Kind,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    /// Bytes allocated (`st_blocks * 512`).
    pub alloc: u64,
    pub mtime: i64,
    /// Subtree totals; equal to the entry's own figures for non-directories.
    pub total_size: u64,
    pub total_alloc: u64,
    pub total_files: u64,
    pub total_dirs: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// `btv2:<hex>` when a current content id is stored for this file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
}

/// One indexed subtree of `/` and the policy for it. All roots share one
/// namespace; what differs between them is policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootSettings {
    pub path: PathBuf,
    #[serde(default = "default_interval")]
    pub interval_minutes: u64,
    /// Every Nth rescan stats every file; the others trust unchanged
    /// directory mtimes. 0 makes every rescan full.
    #[serde(default = "default_full_every")]
    pub full_every: u32,
    #[serde(default = "yes")]
    pub one_filesystem: bool,
    /// gitignore-style patterns, relative to `path`: `/down/big`, `*.iso`.
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default = "yes")]
    pub classify: bool,
    /// Subtrees (relative to `path`, or absolute) whose files get
    /// BitTorrent v2 content ids.
    #[serde(default)]
    pub contentid: Vec<PathBuf>,
}

impl RootSettings {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            interval_minutes: default_interval(),
            full_every: default_full_every(),
            one_filesystem: true,
            exclude: Vec::new(),
            classify: true,
            contentid: Vec::new(),
        }
    }
}

/// Once a day: applications' `invalidate` calls carry the day-to-day
/// changes, so the periodic scan is a safety net.
pub const fn default_interval() -> u64 {
    24 * 60
}

/// At a daily interval every rescan can afford to be full.
pub const fn default_full_every() -> u32 {
    1
}

const fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScanReport {
    pub root: String,
    pub dirs_read: u64,
    pub dirs_trusted: u64,
    pub entries_seen: u64,
    pub inserted: u64,
    pub updated: u64,
    pub deleted: u64,
    pub errors: u64,
    pub millis: u64,
    #[serde(default)]
    pub load_ms: u64,
    #[serde(default)]
    pub write_ms: u64,
    #[serde(default)]
    pub totals_ms: u64,
}

/// A transport failure or an error the daemon returned.
#[derive(Clone, Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self(e.to_string())
    }
}

/// Blocking client for one connection to `stewardd`.
#[derive(Debug)]
pub struct Client {
    reader: std::io::BufReader<std::os::unix::net::UnixStream>,
    writer: std::os::unix::net::UnixStream,
}

impl Client {
    pub fn connect() -> std::io::Result<Self> {
        let path = socket_path();
        let writer = std::os::unix::net::UnixStream::connect(&path).map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!("connecting to stewardd at {}: {e}", path.display()),
            )
        })?;
        Ok(Self {
            reader: std::io::BufReader::new(writer.try_clone()?),
            writer,
        })
    }

    /// Send any JSON request; a daemon error becomes `Err`.
    pub fn call(&mut self, req: &serde_json::Value) -> Result<serde_json::Value, Error> {
        use std::io::{BufRead, Write};
        let mut line = serde_json::to_vec(req).map_err(Error::from)?;
        line.push(b'\n');
        self.writer.write_all(&line).map_err(Error::from)?;
        let mut buf = String::new();
        self.reader.read_line(&mut buf).map_err(Error::from)?;
        match serde_json::from_str(&buf).map_err(Error::from)? {
            Response::Ok { result } => Ok(result),
            Response::Error { message } => Err(Error(message)),
        }
    }

    pub fn request(&mut self, req: &Request) -> Result<serde_json::Value, Error> {
        self.call(&serde_json::to_value(req).map_err(Error::from)?)
    }

    pub fn stat(&mut self, path: PathBuf) -> Result<Entry, Error> {
        serde_json::from_value(self.request(&Request::Stat { path })?).map_err(Error::from)
    }

    pub fn children(&mut self, path: PathBuf) -> Result<Vec<Entry>, Error> {
        serde_json::from_value(self.request(&Request::Children { path })?).map_err(Error::from)
    }

    pub fn locate(&mut self, pattern: String, limit: u32) -> Result<Vec<String>, Error> {
        serde_json::from_value(self.request(&Request::Locate { pattern, limit })?)
            .map_err(Error::from)
    }
}
