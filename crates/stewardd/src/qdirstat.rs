//! Export a subtree as a qdirstat 2.0 cache file (`.qdirstat.cache.gz`),
//! byte-compatible with qdirstat's `CacheWriter` so either tool can read it.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Context, Result};
use flate2::Compression;
use flate2::write::GzEncoder;
use steward_index::{Index, Kind, Record};
use turso::Connection;

/// qdirstat allows this much slack before calling a file sparse.
const FRAGMENT_SIZE: u64 = 2048;

const HEADER: &str = "[qdirstat 2.0 cache file]\n# Do not edit!\n#\n\
    # Type  path                            size     uid   gid  perm.       mtime      <optional fields>\n#\n";

/// Percent-encoding as `QUrl::toEncoded` does for a path.
fn url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        let keep = b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/".contains(&b);
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn size(n: u64) -> String {
    const K: u64 = 1024;
    for (unit, suffix) in [
        (K * K * K * K, 'T'),
        (K * K * K, 'G'),
        (K * K, 'M'),
        (K, 'K'),
    ] {
        if n >= unit && n.is_multiple_of(unit) {
            return format!("{}{suffix}", n / unit);
        }
    }
    n.to_string()
}

fn line(w: &mut impl Write, r: &Record, name: &str, is_top: bool, parent_alloc: u64) -> Result<()> {
    let m = &r.meta;
    let special = matches!(
        m.kind,
        Kind::BlockDev | Kind::CharDev | Kind::Fifo | Kind::Socket | Kind::Other
    );
    let kind = match m.kind {
        Kind::File => "F",
        Kind::Dir => "D",
        Kind::Symlink => "L",
        Kind::BlockDev => "BlockDev",
        Kind::CharDev => "CharDev",
        Kind::Fifo => "FIFO",
        Kind::Socket => "Socket",
        Kind::Other => "",
    };
    if is_top {
        write!(w, "{kind} {name:<30}")?;
    } else {
        write!(w, "{kind}\t{name:<24}")?;
    }
    let bytes = if special { 0 } else { m.size };
    write!(
        w,
        "\t{}\t{}  {}  0{:3o}\t0x{:x}",
        size(bytes),
        m.uid,
        m.gid,
        m.mode,
        m.mtime_ns.div_euclid(1_000_000_000)
    )?;
    if m.kind == Kind::File {
        let blocks = m.alloc / 512;
        // qdirstat trusts st_blocks == 0 only where directories report blocks
        // (not btrfs); elsewhere such a file counts as fully allocated.
        let allocated = if blocks == 0 && m.size > 0 && parent_alloc == 0 {
            m.size
        } else {
            m.alloc
        };
        if allocated + FRAGMENT_SIZE < m.size {
            write!(w, "\tblocks: {blocks}")?;
        }
        if m.nlink > 1 {
            write!(w, "\tlinks: {}", m.nlink)?;
        }
    }
    writeln!(w)?;
    Ok(())
}

/// Write the tree under `id` (at `path`) to `out`; returns entries written.
pub async fn export(
    index: &Index,
    conn: &Connection,
    id: i64,
    path: &Path,
    out: &Path,
) -> Result<u64> {
    let top = index.get(conn, id).await?.context("vanished")?;
    let file = File::create(out).with_context(|| out.display().to_string())?;
    let mut w = GzEncoder::new(
        BufWriter::with_capacity(1 << 20, file),
        Compression::default(),
    );
    w.write_all(HEADER.as_bytes())?;

    let mut n = 0;
    let mut stack = vec![(top, path.to_path_buf())];
    while let Some((dir, dir_path)) = stack.pop() {
        line(
            &mut w,
            &dir,
            &url_encode(dir_path.as_os_str().as_bytes()),
            true,
            0,
        )?;
        n += 1;
        let mut kids = index.children_of(conn, dir.id).await?;
        kids.sort_by(|a, b| a.name.cmp(&b.name));
        let mut subdirs = Vec::new();
        for k in kids {
            if k.meta.kind == Kind::Dir {
                subdirs.push(k);
            } else {
                line(
                    &mut w,
                    &k,
                    &url_encode(k.name.as_bytes()),
                    false,
                    dir.meta.alloc,
                )?;
                n += 1;
            }
        }
        for d in subdirs.into_iter().rev() {
            let p = dir_path.join(&d.name);
            stack.push((d, p));
        }
    }
    w.finish()?.flush()?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_qdirstat_formatting() {
        assert_eq!(
            url_encode("Sex, Drugs (sped up⧸tiktok) [x]".as_bytes()),
            "Sex,%20Drugs%20(sped%20up%E2%A7%B8tiktok)%20%5Bx%5D"
        );
        assert_eq!(size(2048), "2K");
        assert_eq!(size(3 * 1024 * 1024 * 1024), "3G");
        assert_eq!(size(1500), "1500");
    }
}
