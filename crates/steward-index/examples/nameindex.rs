//! What an index on entry names would cost and buy:
//! `cargo run --release --example nameindex -- INDEX.db NAME...`
//! Times exact, prefix and substring lookups, adds the index, times them
//! again and reports the file's growth. Run it on a scratch copy.

use std::time::Instant;

use turso::Value;

async fn count(conn: &turso::Connection, sql: &str, param: Value) -> anyhow::Result<(usize, f64)> {
    let t = Instant::now();
    let mut rows = conn.query(sql, (param,)).await?;
    let mut n = 0;
    while rows.next().await?.is_some() {
        n += 1;
    }
    Ok((n, t.elapsed().as_secs_f64() * 1e3))
}

async fn lookups(conn: &turso::Connection, names: &[String], label: &str) -> anyhow::Result<()> {
    for name in names {
        let (n, ms) = count(
            conn,
            "SELECT id FROM entries WHERE name = ?1",
            Value::Blob(name.as_bytes().to_vec()),
        )
        .await?;
        println!("{label} exact {name:?}: {n} rows in {ms:.1} ms");
        let mut hi = name.as_bytes().to_vec();
        if let Some(last) = hi.last_mut() {
            *last += 1;
        }
        let t = Instant::now();
        let mut rows = conn
            .query(
                "SELECT id FROM entries WHERE name >= ?1 AND name < ?2",
                (Value::Blob(name.as_bytes().to_vec()), Value::Blob(hi)),
            )
            .await?;
        let mut n = 0;
        while rows.next().await?.is_some() {
            n += 1;
        }
        println!(
            "{label} prefix {name:?}: {n} rows in {:.1} ms",
            t.elapsed().as_secs_f64() * 1e3
        );
    }
    let (n, ms) = count(
        conn,
        "SELECT id FROM entries WHERE name LIKE ?1",
        Value::Text(format!("%{}%", names[0])),
    )
    .await?;
    println!("{label} substring {:?}: {n} rows in {ms:.1} ms", names[0]);
    Ok(())
}

fn size(path: &str) -> u64 {
    let wal = format!("{path}-wal");
    std::fs::metadata(path).map_or(0, |m| m.len()) + std::fs::metadata(wal).map_or(0, |m| m.len())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (db, names) = args.split_first().expect("INDEX.db NAME...");
    let index = steward_index::Index::open(std::path::Path::new(db)).await?;
    let conn = index.connect()?;
    let before = size(db);
    lookups(&conn, names, "no index:  ").await?;
    let t = Instant::now();
    conn.execute(
        "CREATE INDEX IF NOT EXISTS entries_name ON entries(name)",
        (),
    )
    .await?;
    println!("built the name index in {:.1} s", t.elapsed().as_secs_f64());
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)", ())
        .await
        .ok();
    let after = size(db);
    println!(
        "file: {:.1} MB -> {:.1} MB (+{:.1} MB, {:.0}%)",
        before as f64 / 1e6,
        after as f64 / 1e6,
        (after - before) as f64 / 1e6,
        (after - before) as f64 * 100.0 / before as f64
    );
    lookups(&conn, names, "with index:").await?;
    Ok(())
}
