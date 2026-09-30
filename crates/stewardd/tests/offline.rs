//! Content on a volume that goes away is offline, not lost, both to
//! `resolve` and to event subscribers.

use std::fs;

use serde_json::{Value, json};
use steward_proto::{ContentRef, Request};
use stewardd::config::Config;
use stewardd::engine::Engine;

#[tokio::test(flavor = "multi_thread")]
async fn unmounted_volume_is_offline_then_online() {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let (data, stash) = (base.join("data"), base.join("stash"));
    let vol = data.join("vol");
    fs::create_dir_all(&vol).unwrap();
    fs::create_dir_all(&stash).unwrap();
    let film = vol.join("film.bin");
    fs::write(&film, vec![7u8; 3 << 20]).unwrap();
    let config = Config::parse(&format!(
        "db = {:?}\n[[root]]\npath = {data:?}\n",
        base.join("index.db")
    ))
    .unwrap();
    let engine = Engine::open(config).await.unwrap();
    engine.refresh(&data, false).await.unwrap();
    let id = engine
        .handle(Request::ContentId { path: film.clone() })
        .await
        .unwrap();
    let id = id.as_str().unwrap().to_string();
    let resolve = |recheck| {
        engine.handle(Request::Resolve {
            contents: vec![ContentRef {
                id: id.clone(),
                size: None,
            }],
            recheck,
        })
    };
    assert_eq!(resolve(true).await.unwrap()[0]["state"], "present");

    // As if `vol` had been indexed on a volume that is now unmounted: its
    // mount point is an empty directory on another filesystem.
    let conn = engine.index.connect().unwrap();
    let vol_id = engine.index.resolve(&conn, &vol).await.unwrap().unwrap();
    let set_dev = |delta: i64| {
        conn.execute(
            "UPDATE entries SET dev = dev + ?1 WHERE id = ?2",
            (delta, vol_id),
        )
    };
    set_dev(1).await.unwrap();
    fs::rename(&film, stash.join("film.bin")).unwrap();

    let r: Value = resolve(true).await.unwrap();
    assert_eq!(r[0]["state"], "offline");
    let obs = &r[0]["observations"][0];
    assert_eq!(obs["online"], false);
    assert_eq!(obs["offline_at"], json!(vol));

    let mut events = engine.events.subscribe(None).live;
    engine.refresh(&data, false).await.unwrap();
    let e = events.recv().await.unwrap();
    assert_eq!(
        (e.name.as_str(), &e.data["path"]),
        ("storage.offline", &json!(vol))
    );
    // Known offline now, without rechecking; nothing was reported lost.
    assert_eq!(resolve(false).await.unwrap()[0]["state"], "offline");
    assert!(events.try_recv().is_err());

    // Mounted again.
    set_dev(-1).await.unwrap();
    fs::rename(stash.join("film.bin"), &film).unwrap();
    engine.refresh(&data, false).await.unwrap();
    let e = events.recv().await.unwrap();
    assert_eq!(
        (e.name.as_str(), &e.data["path"]),
        ("storage.online", &json!(vol))
    );
    assert_eq!(resolve(true).await.unwrap()[0]["state"], "present");
}

/// Another volume mounted where the indexed one was, with the same paths:
/// its files are not the indexed copies, and the copy is offline at the
/// same directory a scan reports.
#[tokio::test(flavor = "multi_thread")]
async fn a_different_volume_at_the_same_mount_point_is_offline() {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let data = base.join("data");
    let vol = data.join("vol");
    fs::create_dir_all(vol.join("sub")).unwrap();
    let film = vol.join("sub/film.bin");
    fs::write(&film, vec![9u8; 3 << 20]).unwrap();
    let config = Config::parse(&format!(
        "db = {:?}\n[[root]]\npath = {data:?}\n",
        base.join("index.db")
    ))
    .unwrap();
    let engine = Engine::open(config).await.unwrap();
    engine.refresh(&data, false).await.unwrap();
    let id = engine
        .handle(Request::ContentId { path: film.clone() })
        .await
        .unwrap();
    let id = id.as_str().unwrap().to_string();

    // Everything under `vol`, and the hashed inode, was indexed on another
    // filesystem than the one mounted there now.
    let conn = engine.index.connect().unwrap();
    let mut entries = Vec::new();
    for p in [&vol, &vol.join("sub"), &film] {
        entries.push(engine.index.resolve(&conn, p).await.unwrap().unwrap());
    }
    for e in entries {
        conn.execute("UPDATE entries SET dev = dev + 1 WHERE id = ?1", (e,))
            .await
            .unwrap();
    }
    conn.execute("UPDATE contentids SET dev = dev + 1", ())
        .await
        .unwrap();
    conn.execute("UPDATE content_entries SET dev = dev + 1", ())
        .await
        .unwrap();

    let r = engine
        .handle(Request::Resolve {
            contents: vec![ContentRef { id, size: None }],
            recheck: true,
        })
        .await
        .unwrap();
    assert_eq!(r[0]["state"], "offline");
    assert_eq!(r[0]["observations"][0]["offline_at"], json!(vol));

    let mut events = engine.events.subscribe(None).live;
    engine.refresh(&data, false).await.unwrap();
    let e = events.recv().await.unwrap();
    assert_eq!(
        (e.name.as_str(), &e.data["path"]),
        ("storage.offline", &json!(vol))
    );
}
