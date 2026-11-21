//! Answers say which scan they are as of, and point at the command that
//! brings them up to date.

use std::fs;

use serde_json::json;
use steward_proto::Request;
use stewardd::config::Config;
use stewardd::engine::Engine;

#[tokio::test(flavor = "multi_thread")]
async fn stat_names_its_scan_and_errors_name_the_next_command() {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let data = base.join("data");
    fs::create_dir_all(data.join("a")).unwrap();
    let note = data.join("a/note.txt");
    fs::write(&note, b"one").unwrap();
    let config = Config::parse(&format!(
        "db = {:?}\n[[root]]\npath = {data:?}\n",
        base.join("index.db")
    ))
    .unwrap();
    let engine = Engine::open(config).await.unwrap();
    engine.refresh(&data, false).await.unwrap();

    let stat = |path| engine.handle(Request::Stat { path });
    let s = stat(note.clone()).await.unwrap();
    assert_eq!(s["scanned"]["path"], json!(data));
    assert_eq!(s["scanned"]["kind"], "full");

    // Scanning a file rereads its folder, and the answer is as of that.
    fs::write(&note, b"one two").unwrap();
    let scan = Request::Scan {
        path: note.clone(),
        trust_dir_mtime: false,
        read_root: false,
    };
    let r = engine.handle(scan).await.unwrap();
    assert_eq!(r["errors"], 0);
    let s = stat(note.clone()).await.unwrap();
    assert_eq!(s["size"], 7);
    assert_eq!(s["scanned"]["path"], json!(data.join("a")));

    // On disk but not indexed yet: say so, and name the scan that adds it.
    fs::create_dir(data.join("a/new")).unwrap();
    let e = stat(data.join("a/new")).await.unwrap_err().to_string();
    assert!(e.contains("exists but is not in the index yet"), "{e}");
    assert!(
        e.contains(&format!("`steward scan {}`", data.join("a").display())),
        "{e}"
    );
    let e = stat(data.join("gone")).await.unwrap_err().to_string();
    assert!(e.contains("does not exist"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn hard_links_are_not_wasted_space() {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let data = base.join("data");
    fs::create_dir_all(&data).unwrap();
    let bytes = vec![5u8; 100_000];
    for f in ["x", "y"] {
        fs::write(data.join(f), &bytes).unwrap();
    }
    fs::hard_link(data.join("x"), data.join("x-link")).unwrap();
    fs::hard_link(data.join("x"), data.join("lone-link")).unwrap();
    fs::write(data.join("solo"), b"different").unwrap();
    fs::hard_link(data.join("solo"), data.join("solo-link")).unwrap();
    let config = Config::parse(&format!(
        "db = {:?}\n[[root]]\npath = {data:?}\n",
        base.join("index.db")
    ))
    .unwrap();
    let engine = Engine::open(config).await.unwrap();
    engine.refresh(&data, false).await.unwrap();
    for f in ["x", "y", "solo"] {
        let path = data.join(f);
        engine.handle(Request::ContentId { path }).await.unwrap();
    }
    engine.refresh(&data, false).await.unwrap();

    let d = engine.duplicates(&data, 10).await.unwrap();
    // `solo` and its link are one inode: no group. x's three links and y: two inodes.
    assert_eq!(d.as_array().unwrap().len(), 1, "{d}");
    assert_eq!(d[0]["wasted"], 100_000);
    assert_eq!(d[0]["paths"].as_array().unwrap().len(), 4);
}
