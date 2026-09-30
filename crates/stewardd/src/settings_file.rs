//! Edits to settings.toml that keep the user's comments and layout.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, value};

use crate::config::{Config, Root, expand};

fn load(path: &Path) -> Result<DocumentMut> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| path.display().to_string()),
    };
    text.parse()
        .with_context(|| format!("parsing {}", path.display()))
}

/// Write via a temporary file and rename, and only if the result parses.
fn save(path: &Path, doc: &DocumentMut) -> Result<()> {
    let text = doc.to_string();
    Config::parse(&text).context("refusing to write settings that would not load")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn roots(doc: &mut DocumentMut) -> &mut ArrayOfTables {
    // `root = []` (explicitly none) or no key at all: start a fresh list.
    if !doc.get("root").is_some_and(Item::is_array_of_tables) {
        doc["root"] = Item::ArrayOfTables(ArrayOfTables::new());
    }
    doc["root"].as_array_of_tables_mut().expect("just ensured")
}

fn table_path(t: &Table) -> Option<PathBuf> {
    t.get("path")
        .and_then(Item::as_str)
        .map(|p| expand(Path::new(p)))
}

/// Home-relative paths are written as `~/…`, as a person would.
fn display(p: &Path) -> String {
    let home = expand(Path::new("~"));
    match p.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

fn set_or_default<T: PartialEq + Into<toml_edit::Value>>(t: &mut Table, key: &str, v: T, d: T) {
    if v == d {
        t.remove(key);
    } else {
        t[key] = value(v);
    }
}

fn fill(t: &mut Table, r: &Root) {
    let d = Root::new(r.path.clone());
    t["path"] = value(display(&r.path));
    set_or_default(
        t,
        "interval_minutes",
        r.interval_minutes as i64,
        d.interval_minutes as i64,
    );
    set_or_default(
        t,
        "full_every",
        i64::from(r.full_every),
        i64::from(d.full_every),
    );
    set_or_default(t, "one_filesystem", r.one_filesystem, d.one_filesystem);
    set_or_default(t, "classify", r.classify, d.classify);
    let list = |items: Vec<String>| {
        let mut a = Array::new();
        for i in items {
            a.push(i);
        }
        a
    };
    if r.exclude.is_empty() {
        t.remove("exclude");
    } else {
        t["exclude"] = value(list(r.exclude.clone()));
    }
    if r.contentid.is_empty() {
        t.remove("contentid");
    } else {
        // Stored relative to the root, as they are usually written.
        let rel = r
            .contentid
            .iter()
            .map(|c| c.strip_prefix(&r.path).unwrap_or(c).display().to_string())
            .collect();
        t["contentid"] = value(list(rel));
    }
}

/// Add `root`, or update the entry with the same path in place.
pub fn put_root(path: &Path, root: &Root) -> Result<()> {
    let mut doc = load(path)?;
    let list = roots(&mut doc);
    let existing = list
        .iter()
        .position(|t| table_path(t).as_deref() == Some(&root.path));
    match existing {
        Some(i) => fill(list.get_mut(i).expect("found"), root),
        None => {
            let mut t = Table::new();
            fill(&mut t, root);
            list.push(t);
        }
    }
    save(path, &doc)
}

/// Remove the root at `root_path`; returns whether it was listed.
pub fn remove_root(path: &Path, root_path: &Path) -> Result<bool> {
    let mut doc = load(path)?;
    if !doc.get("root").is_some_and(Item::is_array_of_tables) {
        bail!("{} lists no roots", path.display());
    }
    let list = roots(&mut doc);
    let Some(i) = list
        .iter()
        .position(|t| table_path(t).as_deref() == Some(root_path))
    else {
        return Ok(false);
    };
    list.remove(i);
    if list.is_empty() {
        // Without this the implicit `$HOME` root would come back.
        doc["root"] = value(Array::new());
    }
    save(path, &doc)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_keep_comments_and_other_roots() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.toml");
        std::fs::write(
            &file,
            "# my notes\n\n[[root]]\n# the big one\npath = \"/srv/a\"\n\n[[root]]\npath = \"/srv/b\"\n",
        )
        .unwrap();
        let mut b = Root::new("/srv/b".into());
        b.exclude = vec!["*.iso".into()];
        b.contentid = vec!["/srv/b/Movies".into()];
        put_root(&file, &b).unwrap();
        put_root(&file, &Root::new("/srv/c".into())).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            text.contains("# my notes") && text.contains("# the big one"),
            "{text}"
        );
        assert!(
            text.contains("exclude = [\"*.iso\"]") && text.contains("contentid = [\"Movies\"]")
        );
        let c = Config::parse(&text).unwrap();
        assert_eq!(c.roots.len(), 3);
        assert_eq!(c.roots[1].contentid, [PathBuf::from("/srv/b/Movies")]);

        for p in ["/srv/a", "/srv/b", "/srv/c"] {
            assert!(remove_root(&file, Path::new(p)).unwrap());
        }
        let c = Config::parse(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert!(
            c.roots.is_empty(),
            "removing every root must not bring back $HOME"
        );
    }
}
