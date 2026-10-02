//! Talking to stewardd from the UI: blocking calls run on the background
//! executor, and every warning the UI shows in colour is logged to stderr.

use steward_proto::{Client, Entry, Error, Request, RootSettings};

/// A daemon call to run off the UI thread.
pub fn fetch<T: Send + 'static>(
    f: impl FnOnce(&mut Client) -> Result<T, Error> + Send + 'static,
) -> impl FnOnce() -> Result<T, String> + Send + 'static {
    move || {
        Client::connect()
            .map_err(|e| e.to_string())
            .and_then(|mut c| f(&mut c).map_err(|e| e.message))
    }
}

/// Echo a warning the UI shows in colour to stderr, so it can be copied.
pub fn print_warning(msg: &str) {
    tracing::warn!("{msg}");
}

/// Echo a status line the UI shows, as `print_warning` does.
pub fn print_info(msg: &str) {
    tracing::info!("{msg}");
}

pub fn scanning(c: &mut Client) -> Result<bool, Error> {
    Ok(c.request(&Request::Status)?["scanning"]
        .as_bool()
        .unwrap_or(false))
}

/// A configured root and what the index currently holds for it.
#[derive(Clone, Debug)]
pub struct RootInfo {
    pub settings: RootSettings,
    pub indexed: Option<Entry>,
    /// Its volume was not mounted at the last scan; the index is unchanged.
    pub offline: bool,
    /// Capacity of the filesystem it is on, as `settings` reports it.
    pub fs: serde_json::Value,
}

pub struct Settings {
    pub file: String,
    pub roots: Vec<RootInfo>,
    pub scanning: bool,
    /// Folder being hashed and the share of its bytes done.
    pub hashing: Option<(String, f32)>,
}

pub fn settings(c: &mut Client) -> Result<Settings, Error> {
    let v = c.request(&Request::Settings)?;
    let roots = v["roots"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            Ok(RootInfo {
                settings: serde_json::from_value(r["settings"].clone())?,
                indexed: serde_json::from_value(r["indexed"].clone())?,
                offline: r["offline"].as_bool().unwrap_or(false),
                fs: r["fs"].clone(),
            })
        })
        .collect::<Result<_, serde_json::Error>>()?;
    Ok(Settings {
        file: v["file"].as_str().unwrap_or_default().to_string(),
        roots,
        scanning: v["scanning"].as_bool().unwrap_or(false),
        hashing: v["hashing"].as_object().map(|h| {
            let n = |k: &str| h.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
            let path = h
                .get("path")
                .and_then(|p| p.as_str())
                .unwrap_or_default()
                .to_string();
            let pct = if n("bytes_total") == 0 {
                100.0
            } else {
                n("bytes_done") as f32 * 100.0 / n("bytes_total") as f32
            };
            (path, pct)
        }),
    })
}

/// Everything the Daemon tab shows, taken at one moment.
pub struct Snapshot {
    pub taken: f64,
    pub status: serde_json::Value,
    /// Per-root settings and state (offline); None if the call failed.
    pub settings: Option<serde_json::Value>,
    /// The daemon's latest events, newest first.
    pub events: Vec<serde_json::Value>,
}

/// Events replayed into the Daemon tab.
const RECENT_EVENTS: u64 = 100;

pub fn snapshot(c: &mut Client) -> Result<Snapshot, Error> {
    let status = c.request(&Request::Status)?;
    let settings = c.request(&Request::Settings).ok();
    let seq = status["activity"]["event_seq"].as_u64().unwrap_or(0);
    Ok(Snapshot {
        taken: crate::format::now(),
        status,
        settings,
        events: recent_events(seq),
    })
}

/// The events up to `seq`, replayed from the daemon's backlog on a
/// connection of their own, newest first. Best effort: what arrived before
/// any failure.
fn recent_events(seq: u64) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    if seq == 0 {
        return out;
    }
    let Ok(c) = Client::connect() else {
        return out;
    };
    if c.set_timeout(Some(std::time::Duration::from_secs(5)))
        .is_err()
    {
        return out;
    }
    let Ok(events) = c.subscribe(Some(seq.saturating_sub(RECENT_EVENTS)), None) else {
        return out;
    };
    for msg in events {
        let Ok(msg) = msg else { break };
        if msg["method"] != "event" {
            break;
        }
        let params = msg["params"].clone();
        let at = params["seq"].as_u64().unwrap_or(u64::MAX);
        out.push(params);
        if at >= seq {
            break;
        }
    }
    out.reverse();
    out
}
