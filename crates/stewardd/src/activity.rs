//! What the daemon is doing right now: the scan in progress, every file
//! being read for hashing, and its clients. Reported by `status` and logged
//! on SIGUSR2.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use serde_json::{Value, json};

type Reads = Arc<Mutex<HashMap<PathBuf, (u64, Instant)>>>;

#[derive(Default)]
pub struct Activity {
    scan: Mutex<Option<(PathBuf, &'static str, Instant)>>,
    reads: Reads,
    pub connections: AtomicU64,
    pub subscribers: AtomicU64,
}

/// Clears the scan entry when the scan ends, however it ends.
pub struct Scanning<'a>(&'a Activity);

impl Drop for Scanning<'_> {
    fn drop(&mut self) {
        *self.0.scan.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

/// Handed to hashing threads; each read is listed while its guard lives.
#[derive(Clone)]
pub struct Readers(Reads);

pub struct Reading(Reads, PathBuf);

impl Drop for Reading {
    fn drop(&mut self) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.1);
    }
}

impl Readers {
    pub fn start(&self, path: &Path, size: u64) -> Reading {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path.to_path_buf(), (size, Instant::now()));
        Reading(Arc::clone(&self.0), path.to_path_buf())
    }
}

/// Counts something for as long as it lives.
pub struct Counted<'a>(&'a AtomicU64);

impl<'a> Counted<'a> {
    pub fn new(n: &'a AtomicU64) -> Self {
        n.fetch_add(1, Ordering::Relaxed);
        Self(n)
    }
}

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Activity {
    pub fn scanning(&self, path: &Path, kind: &'static str) -> Scanning<'_> {
        *self.scan.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((path.to_path_buf(), kind, Instant::now()));
        Scanning(self)
    }

    pub fn readers(&self) -> Readers {
        Readers(Arc::clone(&self.reads))
    }

    pub fn report(&self) -> Value {
        let scan = self
            .scan
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|(path, kind, t)| {
                json!({ "path": path, "kind": kind, "secs": t.elapsed().as_secs() })
            });
        let mut reads: Vec<_> = self
            .reads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(p, (size, t))| (t.elapsed().as_secs(), p.clone(), *size))
            .collect();
        reads.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        json!({
            "scan": scan,
            "reading": reads
                .into_iter()
                .map(|(secs, path, size)| json!({ "path": path, "size": size, "secs": secs }))
                .collect::<Vec<_>>(),
            "connections": self.connections.load(Ordering::Relaxed),
            "subscribers": self.subscribers.load(Ordering::Relaxed),
        })
    }
}
