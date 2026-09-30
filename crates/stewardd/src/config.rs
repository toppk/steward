use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub use steward_proto::RootSettings as Root;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_db")]
    pub db: PathBuf,
    /// Missing means "just `$HOME`"; `root = []` means no roots at all.
    #[serde(default, rename = "root", skip_serializing_if = "Option::is_none")]
    roots_raw: Option<Vec<Root>>,
    #[serde(skip)]
    pub roots: Vec<Root>,
    /// Files hashed at once for content ids; unset means `default_hash_threads`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash_threads: Option<usize>,
}

/// Half the CPUs, capped at 4: hashing is mostly disk-bound, and a few
/// sequential readers beat many seeking ones on spinning disks.
pub fn default_hash_threads() -> usize {
    let cpus = std::thread::available_parallelism().map_or(2, std::num::NonZero::get);
    (cpus / 2).clamp(1, 4)
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var).map_or_else(
        || PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(fallback),
        PathBuf::from,
    )
}

pub fn expand(p: &Path) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) if rest.as_os_str().is_empty() => xdg("HOME", ""),
        Ok(rest) => xdg("HOME", "").join(rest),
        Err(_) => p.to_path_buf(),
    }
}

fn default_db() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("steward/index.db")
}

impl Config {
    pub fn path() -> PathBuf {
        std::env::var_os("STEWARD_CONFIG").map_or_else(
            || xdg("XDG_CONFIG_HOME", ".config").join("steward/settings.toml"),
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
        Self::parse(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut config: Self = toml::from_str(text)?;
        config.db = expand(&config.db);
        config.roots = config
            .roots_raw
            .take()
            .unwrap_or_else(|| vec![Root::new(xdg("HOME", ""))]);
        for r in &mut config.roots {
            r.path = expand(&r.path);
            let base = r.path.clone();
            for c in &mut r.contentid {
                *c = base.join(expand(c));
            }
        }
        Ok(config)
    }

    pub fn hash_threads(&self) -> usize {
        self.hash_threads
            .unwrap_or_else(default_hash_threads)
            .max(1)
    }

    /// The configured root `path` falls under (the deepest one if nested).
    pub fn root_for(&self, path: &Path) -> Option<&Root> {
        self.roots
            .iter()
            .filter(|r| path.starts_with(&r.path))
            .max_by_key(|r| r.path.components().count())
    }

    pub fn covers(&self, path: &Path) -> bool {
        self.roots.iter().any(|r| path.starts_with(&r.path))
    }

    /// The parts of a freshly scanned `path` that want content ids.
    pub fn content_id_targets(&self, path: &Path) -> Vec<PathBuf> {
        let all = self.roots.iter().flat_map(|r| &r.contentid);
        if all.clone().any(|c| path.starts_with(c)) {
            return vec![path.to_path_buf()];
        }
        all.filter(|c| c.starts_with(path)).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_root_policy() {
        let c = Config::parse(
            r#"
            [[root]]
            path = "/home/me"
            exclude = ["/down/big", "*.iso"]

            [[root]]
            path = "/home/media"
            classify = false
            contentid = ["Movies", "TV"]
            "#,
        )
        .unwrap();
        assert_eq!(
            c.root_for(Path::new("/home/media/TV/x")).unwrap().path,
            Path::new("/home/media")
        );
        assert!(!c.covers(Path::new("/srv")));
        assert_eq!(
            c.content_id_targets(Path::new("/home/media")),
            [
                PathBuf::from("/home/media/Movies"),
                PathBuf::from("/home/media/TV")
            ]
        );
        assert_eq!(
            c.content_id_targets(Path::new("/home/media/TV/Show")),
            [PathBuf::from("/home/media/TV/Show")]
        );
        assert!(c.content_id_targets(Path::new("/home/me")).is_empty());
        assert_eq!(c.hash_threads(), default_hash_threads());
        assert_eq!(Config::parse("hash_threads = 8").unwrap().hash_threads(), 8);
        assert_eq!(Config::parse("hash_threads = 0").unwrap().hash_threads(), 1);
    }
}
