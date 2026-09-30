use std::fs;

use super::*;

/// Fixtures are created moments before scanning, so the recent-change
/// guard would mark every directory untrusted; tests of other behaviour
/// turn it off.
fn opts() -> ScanOptions {
    ScanOptions {
        recent_window: std::time::Duration::ZERO,
        ..ScanOptions::default()
    }
}

async fn fixture() -> (tempfile::TempDir, Index, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    fs::create_dir_all(root.join("a/b")).unwrap();
    fs::write(root.join("a/one.txt"), vec![0u8; 5000]).unwrap();
    fs::write(root.join("a/b/two.txt"), b"hello").unwrap();
    fs::write(root.join("top.txt"), b"x").unwrap();
    let index = Index::open(&tmp.path().join("db/index.db")).await.unwrap();
    let root = fs::canonicalize(root).unwrap();
    (tmp, index, root)
}

async fn totals(index: &Index, path: &Path) -> (u64, u64, u64) {
    let conn = index.connect().unwrap();
    let id = index.resolve(&conn, path).await.unwrap().expect("indexed");
    let r = index.get(&conn, id).await.unwrap().unwrap();
    (r.t_size, r.t_files, r.t_dirs)
}

#[tokio::test(flavor = "multi_thread")]
async fn scan_builds_tree_with_totals() {
    let (_tmp, index, root) = fixture().await;
    let stats = index.scan(&root, opts()).await.unwrap();
    assert_eq!(stats.inserted, 6);
    let (_, files, dirs) = totals(&index, &root).await;
    assert_eq!((files, dirs), (3, 3));
    let (_, files, _) = totals(&index, &root.join("a/b")).await;
    assert_eq!(files, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn rescan_writes_only_changes() {
    let (_tmp, index, root) = fixture().await;
    index.scan(&root, opts()).await.unwrap();
    let again = index.scan(&root, opts()).await.unwrap();
    assert_eq!((again.inserted, again.updated, again.deleted), (0, 0, 0));

    fs::remove_dir_all(root.join("a/b")).unwrap();
    fs::write(root.join("a/new.txt"), b"new").unwrap();
    let s = index.scan(&root, opts()).await.unwrap();
    assert_eq!((s.inserted, s.deleted), (1, 2));
    let (_, files, dirs) = totals(&index, &root).await;
    assert_eq!((files, dirs), (3, 2));
}

#[tokio::test(flavor = "multi_thread")]
async fn trusting_scan_skips_unchanged_dirs_but_sees_new_names() {
    let (_tmp, index, root) = fixture().await;
    index.scan(&root, opts()).await.unwrap();
    fs::write(root.join("a/b/three.txt"), b"3").unwrap();
    let opts = ScanOptions {
        trust_dir_mtime: true,
        ..opts()
    };
    let s = index.scan(&root, opts).await.unwrap();
    assert_eq!(s.dirs_read, 1);
    assert_eq!(s.dirs_trusted, 2);
    assert_eq!(s.inserted, 1);
    let (_, files, _) = totals(&index, &root).await;
    assert_eq!(files, 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn subdir_scan_updates_ancestor_totals() {
    let (_tmp, index, root) = fixture().await;
    index.scan(&root, opts()).await.unwrap();
    fs::write(root.join("a/b/big.bin"), vec![1u8; 100_000]).unwrap();
    index.scan(&root.join("a/b"), opts()).await.unwrap();
    let (size, files, _) = totals(&index, &root).await;
    assert_eq!(files, 4);
    assert!(size >= 105_006);
}

#[tokio::test(flavor = "multi_thread")]
async fn wider_scan_adopts_existing_root() {
    let (_tmp, index, root) = fixture().await;
    index.scan(&root.join("a"), opts()).await.unwrap();
    index.scan(&root, opts()).await.unwrap();
    let conn = index.connect().unwrap();
    assert_eq!(index.roots(&conn).await.unwrap().len(), 1);
    let (_, files, _) = totals(&index, &root).await;
    assert_eq!(files, 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn locate_finds_names() {
    let (_tmp, index, root) = fixture().await;
    index.scan(&root, opts()).await.unwrap();
    let conn = index.connect().unwrap();
    let hits = index.locate(&conn, "TWO", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].1, root.join("a/b/two.txt"));
    let hits = index.locate(&conn, "*.txt", 10).await.unwrap();
    assert_eq!(hits.len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn leftover_dirty_set_is_retotalled_by_next_scan() {
    let (_tmp, index, root) = fixture().await;
    index.scan(&root, opts()).await.unwrap();
    let conn = index.connect().unwrap();
    let id = index.resolve(&conn, &root).await.unwrap().unwrap();
    // What an interrupted scan leaves: wrong totals, marked dirty.
    conn.execute(
        "UPDATE entries SET t_files = 0, t_dirs = 0 WHERE id = ?1",
        (id,),
    )
    .await
    .unwrap();
    conn.execute("INSERT INTO dirty (id) VALUES (?1)", (id,))
        .await
        .unwrap();
    index.scan(&root, opts()).await.unwrap();
    let (_, files, dirs) = totals(&index, &root).await;
    assert_eq!((files, dirs), (3, 3));
}

#[tokio::test(flavor = "multi_thread")]
async fn old_totals_version_is_repaired_on_open() {
    let (tmp, index, root) = fixture().await;
    std::os::unix::fs::symlink("top.txt", root.join("link")).unwrap();
    index.scan(&root, opts()).await.unwrap();
    let conn = index.connect().unwrap();
    conn.execute("UPDATE entries SET t_files = 99", ())
        .await
        .unwrap();
    conn.execute("UPDATE meta SET value = 1 WHERE key = 'totals'", ())
        .await
        .unwrap();
    drop(conn);
    drop(index);
    let index = Index::open(&tmp.path().join("db/index.db")).await.unwrap();
    let (_, files, dirs) = totals(&index, &root).await;
    assert_eq!((files, dirs), (3, 3), "the symlink is not a file");
}

#[tokio::test(flavor = "multi_thread")]
async fn excludes_drop_matching_entries() {
    let (_tmp, index, root) = fixture().await;
    index.scan(&root, opts()).await.unwrap();
    let opts = ScanOptions {
        exclude: vec!["/a/b".into(), "top.txt".into()],
        ..opts()
    };
    let s = index.scan(&root, opts).await.unwrap();
    assert_eq!(s.deleted, 3, "a/b, a/b/two.txt, top.txt");
    let (_, files, dirs) = totals(&index, &root).await;
    assert_eq!((files, dirs), (1, 2));
    let conn = index.connect().unwrap();
    assert_eq!(index.remove_root(&root).await.unwrap(), 3);
    assert!(index.roots(&conn).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn recently_changed_dirs_are_reread_by_the_next_trusting_scan() {
    let (_tmp, index, root) = fixture().await;
    // Default guard: everything was just created, so nothing is trusted yet.
    index.scan(&root, ScanOptions::default()).await.unwrap();
    let trusting = ScanOptions {
        trust_dir_mtime: true,
        ..ScanOptions::default()
    };
    let s = index.scan(&root, trusting.clone()).await.unwrap();
    assert_eq!((s.dirs_read, s.dirs_trusted), (3, 0));
    // Once the times are old enough the directories are trusted.
    let old = ScanOptions {
        recent_window: std::time::Duration::ZERO,
        ..trusting
    };
    index.scan(&root, old.clone()).await.unwrap();
    let s = index.scan(&root, old).await.unwrap();
    assert_eq!((s.dirs_read, s.dirs_trusted), (0, 3));
}

#[tokio::test(flavor = "multi_thread")]
async fn subtree_scan_only_loads_that_subtree() {
    let (_tmp, index, root) = fixture().await;
    index.scan(&root, opts()).await.unwrap();
    fs::write(root.join("a/b/late.txt"), b"x").unwrap();
    let s = index.scan(&root.join("a/b"), opts()).await.unwrap();
    assert_eq!(s.inserted, 1);
    let (_, files, _) = totals(&index, &root).await;
    assert_eq!(files, 4);
}
