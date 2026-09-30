use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Root {
    pub path: PathBuf,
    #[serde(default = "default_interval")]
    pub interval_minutes: u64,
    /// Every Nth rescan stats every file; the others trust unchanged
    /// directory mtimes. 0 makes every rescan full.
    #[serde(default = "default_full_every")]
    pub full_every: u32,
    #[serde(default = "yes")]
    pub one_filesystem: bool,
}

const fn default_interval() -> u64 {
    30
}

const fn default_full_every() -> u32 {
    8
}

const fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_db")]
    pub db: PathBuf,
    #[serde(default, rename = "root")]
    pub roots: Vec<Root>,
    /// Subtrees whose files get BitTorrent v2 content ids after each scan.
    #[serde(default)]
    pub contentid: Vec<PathBuf>,
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var).map_or_else(
        || PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(fallback),
        PathBuf::from,
    )
}

fn default_db() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("steward/index.db")
}

impl Config {
    pub fn path() -> PathBuf {
        std::env::var_os("STEWARD_CONFIG").map_or_else(
            || xdg("XDG_CONFIG_HOME", ".config").join("steward/config.toml"),
            PathBuf::from,
        )
    }

    pub fn load() -> Result<Self> {
        let path = Self::path();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| path.display().to_string()),
        };
        let mut config: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if config.roots.is_empty() {
            config.roots.push(Root {
                path: xdg("HOME", ""),
                interval_minutes: default_interval(),
                full_every: default_full_every(),
                one_filesystem: true,
            });
        }
        Ok(config)
    }

    /// The parts of a freshly scanned `path` that want content ids.
    pub fn content_id_targets(&self, path: &Path) -> Vec<PathBuf> {
        if self.contentid.iter().any(|c| path.starts_with(c)) {
            return vec![path.to_path_buf()];
        }
        self.contentid
            .iter()
            .filter(|c| c.starts_with(path))
            .cloned()
            .collect()
    }
}
