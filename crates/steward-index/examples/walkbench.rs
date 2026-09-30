use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::time::Instant;

use steward_index::scan::{Counters, Walker};

#[tokio::main]
async fn main() {
    let root = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    for _ in 0..3 {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4096);
        let w = Walker {
            exclude: None,
            snapshot: Arc::new(HashMap::new()),
            next_id: Arc::new(AtomicI64::new(2)),
            trust_dir_mtime: false,
            always_read: 0,
            one_filesystem: true,
            counters: Arc::new(Counters::default()),
            tx,
        };
        let t = Instant::now();
        let r = root.clone();
        let h = tokio::task::spawn_blocking(move || w.run(&r, 1, 0));
        let mut n = 0;
        while let Some(l) = rx.recv().await {
            n += l.entries.map_or(0, |e| e.len());
        }
        h.await.unwrap();
        println!("walk only: {n} entries in {:?}", t.elapsed());
    }
}
