//! Layer 2: what an entry *is*. Directory-level classes (repositories,
//! ignored build output, caches, trash) are stored as tags on the topmost
//! entry they apply to and inherited by everything beneath it. File-level
//! categories come from the name alone and are derived on read, never stored.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use steward_index::{Index, Kind, Record};

pub const SOURCE: &str = "classify";

/// A well-known directory: its name, a sibling or child that must exist for
/// the name to be trusted, and the tag it earns.
struct Rule {
    name: &'static str,
    sibling: &'static [&'static str],
    child: &'static [&'static str],
    tag: &'static str,
}

const fn rule(name: &'static str, tag: &'static str) -> Rule {
    Rule {
        name,
        sibling: &[],
        child: &[],
        tag,
    }
}

const RULES: &[Rule] = &[
    Rule {
        sibling: &["package.json"],
        ..rule("node_modules", "dependencies")
    },
    Rule {
        sibling: &["Cargo.toml"],
        ..rule("target", "build-output")
    },
    Rule {
        sibling: &[
            "build.gradle",
            "build.gradle.kts",
            "CMakeLists.txt",
            "meson.build",
        ],
        ..rule("build", "build-output")
    },
    Rule {
        sibling: &["package.json", "pyproject.toml", "setup.py"],
        ..rule("dist", "build-output")
    },
    Rule {
        sibling: &["build.zig"],
        ..rule("zig-out", "build-output")
    },
    Rule {
        child: &["pyvenv.cfg"],
        ..rule(".venv", "venv")
    },
    Rule {
        child: &["pyvenv.cfg"],
        ..rule("venv", "venv")
    },
    rule("__pycache__", "cache"),
    rule(".mypy_cache", "cache"),
    rule(".pytest_cache", "cache"),
    rule(".ruff_cache", "cache"),
    rule(".tox", "cache"),
    rule(".nox", "cache"),
    rule(".gradle", "cache"),
    rule(".direnv", "cache"),
    rule(".ccache", "cache"),
    rule(".zig-cache", "build-output"),
    rule("zig-cache", "build-output"),
    rule(".next", "build-output"),
    rule(".nuxt", "build-output"),
    rule(".svelte-kit", "build-output"),
    rule(".parcel-cache", "cache"),
    rule(".turbo", "cache"),
    rule("CMakeFiles", "build-output"),
    rule(".terraform", "dependencies"),
    rule(".cache", "cache"),
];

fn well_known(
    name: &str,
    siblings: &[&str],
    children: &[&str],
    path: &Path,
) -> Option<&'static str> {
    if name.starts_with(".Trash-") || (name == "Trash" && path.ends_with(".local/share/Trash")) {
        return Some("trash");
    }
    RULES
        .iter()
        .find(|r| {
            r.name == name
                && (r.sibling.is_empty() || r.sibling.iter().any(|s| siblings.contains(s)))
                && (r.child.is_empty() || r.child.iter().any(|c| children.contains(c)))
        })
        .map(|r| r.tag)
}

/// A file's broad category from its extension, in the spirit of qdirstat's
/// MIME categories.
pub fn category(name: &OsStr) -> Option<&'static str> {
    let ext = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "heic" | "heif" | "avif" | "tif" | "tiff"
        | "bmp" | "svg" | "raw" | "cr2" | "cr3" | "nef" | "arw" | "dng" | "orf" | "rw2" | "raf"
        | "xcf" | "psd" => "image",
        "mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v" | "mpg" | "mpeg" | "wmv" | "ts" | "m2ts" => {
            "video"
        }
        "mp3" | "flac" | "ogg" | "opus" | "m4a" | "aac" | "wav" | "aiff" | "wma" => "audio",
        "zip" | "tar" | "gz" | "tgz" | "xz" | "txz" | "bz2" | "zst" | "7z" | "rar" | "iso"
        | "rpm" | "deb" => "archive",
        "pdf" | "doc" | "docx" | "odt" | "xls" | "xlsx" | "ods" | "ppt" | "pptx" | "odp"
        | "epub" | "md" | "txt" | "rtf" => "document",
        "rs" | "c" | "h" | "cc" | "cpp" | "hpp" | "py" | "js" | "mjs" | "go" | "java" | "kt"
        | "rb" | "sh" | "zig" | "swift" => "source",
        "o" | "a" | "so" | "rlib" | "rmeta" | "pyc" | "class" | "obj" => "object",
        "qcow2" | "vmdk" | "vdi" | "img" => "disk-image",
        "torrent" => "torrent",
        _ => return None,
    })
}

#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub scanned: usize,
    pub tagged: usize,
}

struct Tree {
    records: Vec<(Record, PathBuf)>,
    children: HashMap<i64, Vec<usize>>,
}

/// Matchers in effect for a directory, outermost first.
#[derive(Clone, Default)]
struct IgnoreStack {
    in_repo: bool,
    matchers: Vec<std::sync::Arc<Gitignore>>,
}

impl IgnoreStack {
    fn ignored(&self, path: &Path, is_dir: bool) -> bool {
        for m in self.matchers.iter().rev() {
            match m.matched(path, is_dir) {
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
                Match::None => {}
            }
        }
        false
    }

    fn with_file(&self, root: &Path, files: &[PathBuf]) -> Self {
        let mut b = GitignoreBuilder::new(root);
        let mut any = false;
        for f in files {
            if f.is_file() {
                any |= b.add(f).is_none();
            }
        }
        let mut next = self.clone();
        if any && let Ok(g) = b.build() {
            next.matchers.push(std::sync::Arc::new(g));
        }
        next
    }
}

/// Recompute this classifier's tags for everything under `root`.
pub async fn classify(index: &Index, root: &Path) -> Result<Summary> {
    let conn = index.connect()?;
    let root = std::path::absolute(root)?;
    let id = index
        .resolve(&conn, &root)
        .await?
        .with_context(|| format!("{} is not indexed", root.display()))?;
    let records = index.subtree(&conn, id, &root).await?;
    let mut children: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, (r, _)) in records.iter().enumerate().skip(1) {
        children.entry(r.parent).or_default().push(i);
    }
    let tree = Tree { records, children };

    let mut tags = Vec::new();
    let mut stack = vec![(0usize, IgnoreStack::default())];
    while let Some((i, ctx)) = stack.pop() {
        let (rec, path) = &tree.records[i];
        let kids = tree.children.get(&rec.id).map_or(&[][..], Vec::as_slice);
        let names: Vec<&str> = kids
            .iter()
            .filter_map(|&k| tree.records[k].0.name.to_str())
            .collect();

        let mut ctx = ctx;
        if names.contains(&".git") {
            tags.push((rec.id, "repo".to_string()));
            ctx = IgnoreStack {
                in_repo: true,
                matchers: vec![],
            }
            .with_file(
                path,
                &[path.join(".git/info/exclude"), path.join(".gitignore")],
            );
        } else if ctx.in_repo && names.contains(&".gitignore") {
            ctx = ctx.with_file(path, &[path.join(".gitignore")]);
        }

        for &k in kids {
            let (child, cpath) = &tree.records[k];
            let is_dir = child.meta.kind == Kind::Dir;
            let name = child.name.to_string_lossy();
            let tag = if is_dir && name == ".git" {
                Some("vcs-metadata")
            } else if is_dir {
                let grandkids: Vec<&str> = tree
                    .children
                    .get(&child.id)
                    .map_or(&[][..], Vec::as_slice)
                    .iter()
                    .filter_map(|&g| tree.records[g].0.name.to_str())
                    .collect();
                well_known(&name, &names, &grandkids, cpath)
            } else {
                None
            };
            if let Some(tag) = tag {
                tags.push((child.id, tag.to_string()));
            }
            let ignored = ctx.in_repo && ctx.ignored(cpath, is_dir);
            if ignored {
                tags.push((child.id, "ignored".to_string()));
            }
            // Below a classified or ignored directory everything inherits.
            if is_dir && tag.is_none() && !ignored {
                stack.push((k, ctx.clone()));
            }
        }
    }

    let scope: Vec<i64> = tree.records.iter().map(|(r, _)| r.id).collect();
    index.replace_tags(&conn, SOURCE, &scope, &tags).await?;
    Ok(Summary {
        scanned: scope.len(),
        tagged: tags.len(),
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use steward_index::ScanOptions;

    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn tags_repo_ignored_and_well_known() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap().join("tree");
        let repo = root.join("proj");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("target/debug")).unwrap();
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::create_dir_all(repo.join("logs")).unwrap();
        fs::write(repo.join("Cargo.toml"), "").unwrap();
        fs::write(repo.join(".gitignore"), "*.log\n/logs/\n").unwrap();
        fs::write(repo.join("src/debug.log"), "").unwrap();
        fs::write(repo.join("src/main.rs"), "").unwrap();
        fs::create_dir_all(root.join("elsewhere/target")).unwrap();

        let index = Index::open(&tmp.path().join("i.db")).await.unwrap();
        index.scan(&root, ScanOptions::default()).await.unwrap();
        classify(&index, &root).await.unwrap();

        let conn = index.connect().unwrap();
        let tags = async |p: &str| {
            let id = index.resolve(&conn, &root.join(p)).await.unwrap().unwrap();
            index.tags_of(&conn, id).await.unwrap()
        };
        assert_eq!(tags("proj").await, ["classify:repo"]);
        assert_eq!(tags("proj/.git").await, ["classify:vcs-metadata"]);
        assert!(
            tags("proj/target")
                .await
                .contains(&"classify:build-output".into())
        );
        assert_eq!(tags("proj/logs").await, ["classify:ignored"]);
        assert_eq!(tags("proj/src/debug.log").await, ["classify:ignored"]);
        assert!(tags("proj/src/main.rs").await.is_empty());
        assert!(tags("elsewhere/target").await.is_empty());

        let id = index
            .resolve(&conn, &repo.join("target/debug"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            index
                .effective_tags(&conn, id)
                .await
                .unwrap()
                .contains(&"classify:build-output".into())
        );
    }

    #[test]
    fn categories() {
        assert_eq!(category(OsStr::new("IMG_1.CR3")), Some("image"));
        assert_eq!(category(OsStr::new("Makefile")), None);
    }
}
