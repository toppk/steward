//! Content and storage events: numbered, fanned out to subscribers, and kept
//! in a bounded backlog so a reconnecting client can resume from a `seq`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;
use steward_proto::EventRecord;
use tokio::sync::broadcast;

const BACKLOG: usize = 4096;
/// Events a slow subscriber may fall behind by before it is told of a gap.
const LAG: usize = 1024;

pub struct Bus {
    inner: Mutex<VecDeque<Arc<EventRecord>>>,
    seq: Mutex<u64>,
    tx: broadcast::Sender<Arc<EventRecord>>,
    /// Changes with every daemon start: a `seq` means nothing across them.
    pub epoch: String,
}

/// What a subscription starts with: the replayed events, whether they reach
/// back to the requested `since`, and the live stream after them.
pub struct Start {
    pub replay: Vec<Arc<EventRecord>>,
    pub complete: bool,
    pub seq: u64,
    pub live: broadcast::Receiver<Arc<EventRecord>>,
}

impl Default for Bus {
    fn default() -> Self {
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        Self {
            inner: Mutex::new(VecDeque::with_capacity(BACKLOG)),
            seq: Mutex::new(0),
            tx: broadcast::channel(LAG).0,
            epoch: format!("{epoch:x}-{}", std::process::id()),
        }
    }
}

impl Bus {
    pub fn emit(&self, name: &str, data: Value) {
        // Numbering, backlog and send happen under one lock, so a subscriber
        // sees each event exactly once: in its replay or on its receiver.
        let mut seq = self.seq.lock().unwrap_or_else(PoisonError::into_inner);
        *seq += 1;
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64());
        let record = Arc::new(EventRecord {
            seq: *seq,
            time,
            name: name.to_string(),
            data,
        });
        tracing::trace!("event {} {}: {}", record.seq, record.name, record.data);
        let mut backlog = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if backlog.len() == BACKLOG {
            backlog.pop_front();
        }
        backlog.push_back(Arc::clone(&record));
        let _ = self.tx.send(record);
    }

    /// The last event's number.
    pub fn seq(&self) -> u64 {
        *self.seq.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn subscribe(&self, since: Option<u64>) -> Start {
        let seq = self.seq.lock().unwrap_or_else(PoisonError::into_inner);
        let backlog = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let live = self.tx.subscribe();
        let (replay, complete) = match since {
            None => (Vec::new(), true),
            Some(since) => {
                let oldest = backlog.front().map_or(*seq + 1, |r| r.seq);
                let complete = since <= *seq && oldest <= since + 1;
                let replay = backlog.iter().filter(|r| r.seq > since).cloned().collect();
                (replay, complete)
            }
        };
        Start {
            replay,
            complete,
            seq: *seq,
            live,
        }
    }
}

/// Whether a subscription filtered to `ids` wants `record`: storage events
/// always, content events only for those contents.
pub fn wanted(ids: Option<&std::collections::HashSet<String>>, record: &EventRecord) -> bool {
    match ids {
        Some(ids) if record.name.starts_with("content.") => record.data["id"]
            .as_str()
            .is_some_and(|id| ids.contains(id)),
        _ => true,
    }
}
