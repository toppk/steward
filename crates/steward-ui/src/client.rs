//! Talking to stewardd from the UI: blocking calls run on the background
//! executor, and every message the UI shows in colour is echoed to stderr.

use steward_proto::{Client, Entry, Error, Request, RootSettings};

/// A daemon call to run off the UI thread.
pub fn fetch<T: Send + 'static>(
    f: impl FnOnce(&mut Client) -> Result<T, Error> + Send + 'static,
) -> impl FnOnce() -> Result<T, String> + Send + 'static {
    move || {
        Client::connect()
            .map_err(|e| e.to_string())
            .and_then(|mut c| f(&mut c).map_err(|e| e.0))
    }
}

/// Echo what a coloured status line shows, so it can be copied.
pub fn print_line(level: &str, msg: &str) {
    let argv0 = std::env::args().next().unwrap_or_default();
    let name = std::path::Path::new(&argv0)
        .file_name()
        .map_or_else(|| "steward-ui".into(), |n| n.to_string_lossy().into_owned());
    eprintln!("{name}: {level}{msg}");
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
