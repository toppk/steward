//! A hashing job must survive scans committing on another connection while
//! it saves its batches.

use std::fs;

use stewardd::config::Config;
use stewardd::engine::Engine;

#[tokio::test(flavor = "multi_thread")]
async fn hashing_survives_concurrent_scan_commits() {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let (media, churn) = (base.join("data/media"), base.join("data/churn"));
    fs::create_dir_all(&media).unwrap();
    fs::create_dir_all(&churn).unwrap();
    for i in 0..3000u32 {
        fs::write(media.join(format!("{i}.bin")), i.to_le_bytes().repeat(512)).unwrap();
    }
    let data = base.join("data");
    let config = Config::parse(&format!(
        "db = {:?}\n[[root]]\npath = {data:?}\n",
        base.join("index.db")
    ))
    .unwrap();
    let engine = std::sync::Arc::new(Engine::open(config).await.unwrap());
    engine.refresh(&data, false).await.unwrap();

    let scans = {
        let (engine, churn) = (std::sync::Arc::clone(&engine), churn.clone());
        tokio::spawn(async move {
            for round in 0..40u32 {
                for i in 0..200u32 {
                    let p = churn.join(format!("{round}-{i}"));
                    fs::write(&p, b"x").unwrap();
                }
                engine.refresh(&churn, false).await.unwrap();
            }
        })
    };
    let hashed = engine.hash_tree(&media).await.unwrap();
    scans.await.unwrap();
    assert_eq!(hashed["hashed"], 3000);
}
