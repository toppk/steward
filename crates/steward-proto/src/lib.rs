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
}
