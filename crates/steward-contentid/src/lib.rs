//! Content identity as BitTorrent v2 (BEP 52) defines it: a SHA-256 merkle
//! tree over 16 KiB blocks. The root ("pieces root") depends only on the bytes,
//! not on the piece length, so it doubles as a stable content id and lets a
//! torrent be built from an index without re-reading the data.

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use sha2::{Digest, Sha256};

pub const BLOCK_SIZE: usize = 16 * 1024;

pub type Hash = [u8; 32];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContentId {
    pub root: Hash,
    pub size: u64,
}

impl ContentId {
    pub fn to_hex(&self) -> String {
        hex(&self.root)
    }

    pub fn from_hex(s: &str, size: u64) -> Option<Self> {
        let s = s.strip_prefix("btv2:").unwrap_or(s);
        if s.len() != 64 {
            return None;
        }
        let mut root = [0u8; 32];
        for (i, b) in root.iter_mut().enumerate() {
            *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
        }
        Some(Self { root, size })
    }
}

impl fmt::Display for ContentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "btv2:{}", self.to_hex())
    }
}

pub fn hex(bytes: &[u8]) -> String {
    use fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn sha(data: &[u8]) -> Hash {
    Sha256::digest(data).into()
}

fn join(left: &Hash, right: &Hash) -> Hash {
    let mut h = Sha256::new();
    h.update(left);
    h.update(right);
    h.finalize().into()
}

/// Builds the tree incrementally so a file never has to fit in memory; only
/// the leaf layer (32 bytes per 16 KiB) is kept.
#[derive(Debug, Default)]
pub struct Hasher {
    leaves: Vec<Hash>,
    partial: Vec<u8>,
    size: u64,
}

impl Hasher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.size += data.len() as u64;
        if !self.partial.is_empty() {
            let take = (BLOCK_SIZE - self.partial.len()).min(data.len());
            self.partial.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.partial.len() == BLOCK_SIZE {
                self.leaves.push(sha(&self.partial));
                self.partial.clear();
            }
        }
        let mut chunks = data.chunks_exact(BLOCK_SIZE);
        for block in &mut chunks {
            self.leaves.push(sha(block));
        }
        self.partial.extend_from_slice(chunks.remainder());
    }

    /// `None` for an empty file, which BEP 52 gives no pieces root.
    pub fn finish(mut self) -> Option<ContentId> {
        if !self.partial.is_empty() {
            self.leaves.push(sha(&self.partial));
        }
        if self.leaves.is_empty() {
            return None;
        }
        Some(ContentId {
            root: merkle_root(&self.leaves),
            size: self.size,
        })
    }
}

/// Root of `leaves` padded to a power of two with zero hashes, per BEP 52.
pub fn merkle_root(leaves: &[Hash]) -> Hash {
    let mut layer = leaves.to_vec();
    let mut pad = [0u8; 32];
    while layer.len() > 1 {
        if layer.len() % 2 == 1 {
            layer.push(pad);
        }
        layer = layer.chunks_exact(2).map(|p| join(&p[0], &p[1])).collect();
        pad = join(&pad, &pad);
    }
    layer[0]
}

pub fn hash_reader(mut r: impl Read) -> io::Result<Option<ContentId>> {
    let mut hasher = Hasher::new();
    let mut buf = vec![0u8; 64 * BLOCK_SIZE];
    loop {
        match r.read(&mut buf) {
            Ok(0) => return Ok(hasher.finish()),
            Ok(n) => hasher.update(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

pub fn hash_file(path: &Path) -> io::Result<Option<ContentId>> {
    hash_reader(File::open(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cid(data: &[u8]) -> Option<ContentId> {
        hash_reader(data).unwrap()
    }

    #[test]
    fn empty_has_no_root() {
        assert_eq!(cid(b""), None);
    }

    #[test]
    fn single_block_root_is_leaf_hash() {
        assert_eq!(cid(b"abc").unwrap().root, sha(b"abc"));
    }

    #[test]
    fn three_blocks_pad_with_zero_leaf() {
        let data = vec![7u8; BLOCK_SIZE * 2 + 5];
        let a = sha(&data[..BLOCK_SIZE]);
        let c = sha(&data[BLOCK_SIZE * 2..]);
        let want = join(&join(&a, &a), &join(&c, &[0; 32]));
        assert_eq!(cid(&data).unwrap().root, want);
    }

    #[test]
    fn five_blocks_pad_upper_layer_with_hashed_zeros() {
        let data: Vec<u8> = (0..BLOCK_SIZE * 5).map(|i| i as u8).collect();
        let l: Vec<Hash> = data.chunks(BLOCK_SIZE).map(sha).collect();
        let z = [0; 32];
        let zz = join(&z, &z);
        let left = join(&join(&l[0], &l[1]), &join(&l[2], &l[3]));
        let right = join(&join(&l[4], &z), &zz);
        assert_eq!(cid(&data).unwrap().root, join(&left, &right));
    }

    #[test]
    fn chunking_does_not_change_result() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i * 31) as u8).collect();
        let mut h = Hasher::new();
        for piece in data.chunks(777) {
            h.update(piece);
        }
        assert_eq!(h.finish(), cid(&data));
    }

    #[test]
    fn hex_roundtrip() {
        let id = cid(b"hello").unwrap();
        assert_eq!(ContentId::from_hex(&id.to_string(), 5), Some(id));
    }
}
