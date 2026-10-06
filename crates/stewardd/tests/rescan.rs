//! Rescanning several folders as one job still classifies what it finds,
//! and records one entry in the scan history.

use std::fs;

use steward_proto::Request;
use stewardd::config::Config;
use stewardd::engine::Engine;

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_rescan_classifies_once_and_records_one_scan() {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let data = base.join("data");
    let repo = data.join("repo");
    for d in ["a", "b", "c"] {
        fs::create_dir_all(repo.join(d)).unwrap();
        fs::write(repo.join(d).join("old.txt"), b"x").unwrap();
    }
    fs::create_dir(repo.join(".git")).unwrap();
    let config = Config::parse(&format!(
        "db = {:?}\n[[root]]\npath = {data:?}\n",
        base.join("index.db")
    ))
    .unwrap();
    let engine = Engine::open(config).await.unwrap();
    engine.refresh(&data, false).await.unwrap();

    // Behind the index's back: each folder changes, and one gains a crate
    // with build output.
    for d in ["a", "b", "c"] {
        fs::remove_file(repo.join(d).join("old.txt")).unwrap();
    }
    fs::write(repo.join("b/Cargo.toml"), b"[package]").unwrap();
    fs::create_dir(repo.join("b/target")).unwrap();

    let dirs: Vec<_> = ["a", "b", "c"].iter().map(|d| repo.join(d)).collect();
    let done = std::sync::Mutex::new(Vec::new());
    engine
        .rescan_folders(
            &dirs,
            |_, _| {},
            |d, _| done.lock().unwrap().push(d.to_path_buf()),
        )
        .await
        .unwrap();
    assert_eq!(*done.lock().unwrap(), dirs);

    let target = engine
        .handle(Request::Stat {
            path: repo.join("b/target"),
        })
        .await
        .unwrap();
    let tags: Vec<_> = target["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    assert!(tags.contains(&"classify:build-output"), "{tags:?}");
    let gone = engine
        .handle(Request::Stat {
            path: repo.join("a/old.txt"),
        })
        .await
        .unwrap_err();
    assert_eq!(stewardd::engine::kind_of(&gone), "not_indexed");

    let status = engine.handle(Request::Status).await.unwrap();
    let last = &status["recent_scans"][0];
    assert_eq!(last["kind"], "folders");
    assert_eq!(last["root"], repo.to_str().unwrap());
    assert_eq!(last["deleted"], 3);
}
