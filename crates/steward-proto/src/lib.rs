//! Wire protocol between `stewardd` and its clients: JSON-RPC 2.0, one
//! message per line over a Unix stream socket. Every request with an `id` gets
//! a response with that `id`; events arrive as `event` notifications on
//! connections that subscribed.
//!
//! Two sockets, split by capability, not by client:
//! - `api.socket`: administration (roots, reload, exports, maintenance);
//! - `content.socket`: application-facing content primitives only.

use std::path::PathBuf;

/// This build's version: the release tag (`v0.2.0`) for release builds,
/// which set `STEWARD_VERSION` when compiling, and `dev` otherwise.
pub const VERSION: &str = match option_env!("STEWARD_VERSION") {
    Some(v) => v,
    None => "dev",
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map_or_else(
        || std::env::temp_dir().join(format!("steward-{}", uid())),
        PathBuf::from,
    )
}

/// The administrative socket.
pub fn socket_path() -> PathBuf {
    runtime_dir().join("steward").join("api.socket")
}

/// The application-facing socket: catalog reads and content primitives.
pub fn content_socket_path() -> PathBuf {
    runtime_dir().join("steward").join("content.socket")
}

fn uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").map_or(0, |m| m.uid())
}

/// A method call; serialises as JSON-RPC `{"method": ..., "params": {...}}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
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
    /// Entries whose final name component matches `pattern`. With
    /// `check`, each result is checked on disk (and with `rescan`, the
    /// folders of stale ones are rescanned and the query run again); the
    /// result is then `{paths, stale, rescanned}` instead of a list.
    Locate {
        pattern: String,
        #[serde(default = "default_limit")]
        limit: u32,
        #[serde(default)]
        mode: LocateMode,
        #[serde(default)]
        ignore_case: bool,
        #[serde(default)]
        kind: Option<Kind>,
        #[serde(default)]
        check: LocateCheck,
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
    /// The BEP-52 piece layer of content `id` for a power-of-two
    /// `piece_size` of at least 1 MiB, as concatenated hex SHA-256 hashes;
    /// empty when the file is no bigger than one piece.
    PieceLayer {
        id: String,
        piece_size: u64,
    },
    /// Current observations of each content id: where copies are, whether
    /// they are reachable. `recheck` re-stats each path first and drops any
    /// that no longer hold that content.
    Resolve {
        contents: Vec<ContentRef>,
        #[serde(default)]
        recheck: bool,
    },
    /// These paths may have changed: bring the catalog up to date for them
    /// now and establish the content ids of the regular files among them
    /// (directories are inspected with everything under them).
    Inspect {
        paths: Vec<PathBuf>,
    },
    /// There is reason to believe the file at `path` no longer holds content
    /// `id`. Steward stops reporting that observation, rereads the file and
    /// records what it holds. `reason` is logged, not interpreted.
    Verify {
        id: String,
        path: PathBuf,
        #[serde(default)]
        reason: String,
    },
    /// Deliver events on this connection from now on (after replaying those
    /// after `since`, when given). `ids` limits content events to those
    /// contents; storage events always arrive. Subscribing again replaces
    /// the filter.
    Subscribe {
        #[serde(default)]
        since: Option<u64>,
        #[serde(default)]
        ids: Option<Vec<String>>,
    },
    Unsubscribe,
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

/// How `locate` matches names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocateMode {
    /// A glob if the pattern has `*`, `?` or `[`, else a substring.
    #[default]
    Auto,
    /// Anywhere in the name, ignoring ASCII case.
    Substring,
    /// The whole name, literally.
    Exact,
    /// A glob over the whole name.
    Glob,
    /// A regular expression anywhere in the name (Rust `regex` syntax).
    Regex,
}

/// Whether `locate` confirms its results on disk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocateCheck {
    /// Answer from the index alone.
    #[default]
    None,
    /// `lstat` each result; report the ones that are gone.
    Exists,
    /// As `exists`, then rescan the folders of the gone ones and ask again.
    Rescan,
}

/// `locate`'s answer when a check was asked for.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Located {
    /// Results that exist on disk.
    pub paths: Vec<String>,
    /// Results the index had that are gone (after any rescan).
    pub stale: Vec<String>,
    /// Folders rescanned to bring the index up to date.
    pub rescanned: Vec<String>,
    /// Folders queued for a rescan rather than rescanned now (past 64).
    #[serde(default)]
    pub queued: Vec<String>,
    /// The search stopped at `limit`: there may be more matches.
    #[serde(default)]
    pub limited: bool,
    /// Milliseconds searching the index (both passes, after a rescan).
    #[serde(default)]
    pub search_ms: f64,
    /// Milliseconds checking results on disk.
    #[serde(default)]
    pub check_ms: f64,
    /// Milliseconds rescanning folders.
    #[serde(default)]
    pub rescan_ms: f64,
}

/// A content id with, optionally, the size the caller expects it to have.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentRef {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

pub const JSONRPC: &str = "2.0";

/// JSON-RPC error codes: the standard ones, and one for every
/// application-level failure, told apart by `data.type`.
pub mod code {
    pub const PARSE: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const FAILED: i64 = -32000;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl RpcRequest {
    /// The typed request, or why it can't be one.
    pub fn request(&self) -> Result<Request, RpcError> {
        let mut obj = serde_json::Map::new();
        obj.insert("method".into(), Value::String(self.method.clone()));
        // Methods without parameters accept `params` absent, null or `{}`.
        match &self.params {
            None | Some(Value::Null) => {}
            Some(Value::Object(m)) if m.is_empty() => {}
            Some(p) => {
                obj.insert("params".into(), p.clone());
            }
        }
        serde_json::from_value(Value::Object(obj)).map_err(|e| {
            let msg = e.to_string();
            if msg.starts_with("unknown variant") {
                RpcError::new(code::METHOD_NOT_FOUND, "method_not_found", msg)
            } else {
                RpcError::new(code::INVALID_PARAMS, "invalid_params", msg)
            }
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

/// `data.type` is a stable string for programs; `message` is for people.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, kind: &str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(serde_json::json!({ "type": kind })),
        }
    }

    pub fn kind(&self) -> &str {
        self.data
            .as_ref()
            .and_then(|d| d["type"].as_str())
            .unwrap_or("failed")
    }
}

/// One event, delivered as the params of an `event` notification.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventRecord {
    pub seq: u64,
    pub time: f64,
    pub name: String,
    pub data: Value,
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
    /// Entries beneath, itself included: files, directories, symlinks and
    /// the rest. Roughly the inodes the subtree uses (hard links count once
    /// per link). 1 for non-directories.
    #[serde(default)]
    pub total_items: u64,
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
    /// Known paths whose volume was not mounted; left as indexed.
    #[serde(default)]
    pub offline: Vec<String>,
    #[serde(default)]
    pub load_ms: u64,
    #[serde(default)]
    pub write_ms: u64,
    #[serde(default)]
    pub totals_ms: u64,
}

/// A transport failure, or an error the daemon returned (`kind` is its
/// stable `data.type`).
#[derive(Clone, Debug)]
pub struct Error {
    pub kind: String,
    pub message: String,
}

impl Error {
    fn transport(message: impl std::fmt::Display) -> Self {
        Self {
            kind: "transport".into(),
            message: message.to_string(),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::transport(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::transport(e)
    }
}

impl From<RpcError> for Error {
    fn from(e: RpcError) -> Self {
        Self {
            kind: e.kind().to_string(),
            message: e.message,
        }
    }
}

/// Blocking client for one connection to `stewardd`.
#[derive(Debug)]
pub struct Client {
    reader: std::io::BufReader<std::os::unix::net::UnixStream>,
    writer: std::os::unix::net::UnixStream,
    next_id: u64,
}

impl Client {
    /// Connect to the administrative socket.
    pub fn connect() -> std::io::Result<Self> {
        Self::connect_to(&socket_path())
    }

    pub fn connect_to(path: &std::path::Path) -> std::io::Result<Self> {
        let writer = std::os::unix::net::UnixStream::connect(path).map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!(
                    "connecting to the steward daemon at {}: {e}",
                    path.display()
                ),
            )
        })?;
        Ok(Self {
            reader: std::io::BufReader::new(writer.try_clone()?),
            writer,
            next_id: 1,
        })
    }

    /// Give up on a read after `timeout` (`None`: wait forever).
    pub fn set_timeout(&self, timeout: Option<std::time::Duration>) -> std::io::Result<()> {
        self.writer.set_read_timeout(timeout)
    }

    /// Call `method` with `params`; a daemon error becomes `Err`.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, Error> {
        use std::io::{BufRead, Write};
        let id = self.next_id;
        self.next_id += 1;
        let req = RpcRequest {
            jsonrpc: JSONRPC.into(),
            id: Some(id.into()),
            method: method.into(),
            params: Some(params),
        };
        let mut line = serde_json::to_vec(&req)?;
        line.push(b'\n');
        self.writer.write_all(&line)?;
        loop {
            let mut buf = String::new();
            if self.reader.read_line(&mut buf)? == 0 {
                return Err(Error::transport("stewardd closed the connection"));
            }
            let msg: Value = serde_json::from_str(&buf)?;
            // Skip notifications; this client does not subscribe.
            if msg.get("id") != Some(&Value::from(id)) {
                continue;
            }
            let resp: RpcResponse = serde_json::from_value(msg)?;
            return match (resp.result, resp.error) {
                (_, Some(e)) => Err(e.into()),
                (Some(r), None) => Ok(r),
                (None, None) => Ok(Value::Null),
            };
        }
    }

    /// Turn this connection into an event stream: `start` is the subscribe
    /// result (`epoch`, `seq`, `complete`); iterating yields each `event` or
    /// `gap` notification as `{"method": ..., "params": ...}`.
    pub fn subscribe(
        mut self,
        since: Option<u64>,
        ids: Option<Vec<String>>,
    ) -> Result<Events, Error> {
        let start = self.request(&Request::Subscribe { since, ids })?;
        Ok(Events {
            client: self,
            start,
        })
    }

    pub fn request(&mut self, req: &Request) -> Result<Value, Error> {
        let v = serde_json::to_value(req)?;
        let method = v["method"].as_str().unwrap_or_default().to_string();
        self.call(
            &method,
            v.get("params")
                .cloned()
                .unwrap_or(Value::Object(Default::default())),
        )
    }

    pub fn stat(&mut self, path: PathBuf) -> Result<Entry, Error> {
        serde_json::from_value(self.request(&Request::Stat { path })?).map_err(Error::from)
    }

    pub fn children(&mut self, path: PathBuf) -> Result<Vec<Entry>, Error> {
        serde_json::from_value(self.request(&Request::Children { path })?).map_err(Error::from)
    }

    pub fn locate(&mut self, pattern: String, limit: u32) -> Result<Vec<String>, Error> {
        serde_json::from_value(self.request(&Request::Locate {
            pattern,
            limit,
            mode: LocateMode::Auto,
            ignore_case: false,
            kind: None,
            check: LocateCheck::None,
        })?)
        .map_err(Error::from)
    }

    /// `locate` with a check (never `None`): results confirmed on disk.
    #[allow(clippy::too_many_arguments)]
    pub fn locate_checked(
        &mut self,
        pattern: String,
        limit: u32,
        mode: LocateMode,
        ignore_case: bool,
        kind: Option<Kind>,
        check: LocateCheck,
    ) -> Result<Located, Error> {
        serde_json::from_value(self.request(&Request::Locate {
            pattern,
            limit,
            mode,
            ignore_case,
            kind,
            check,
        })?)
        .map_err(Error::from)
    }
}

/// Notifications from a subscribed connection.
#[derive(Debug)]
pub struct Events {
    client: Client,
    pub start: Value,
}

impl Iterator for Events {
    type Item = Result<Value, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        use std::io::BufRead;
        let mut buf = String::new();
        match self.client.reader.read_line(&mut buf) {
            Ok(0) => None,
            Ok(_) => Some(
                serde_json::from_str::<Value>(&buf)
                    .map_err(Error::from)
                    .map(|mut v| json_take(&mut v)),
            ),
            Err(e) => Some(Err(e.into())),
        }
    }
}

fn json_take(v: &mut Value) -> Value {
    serde_json::json!({ "method": v["method"].take(), "params": v["params"].take() })
}
