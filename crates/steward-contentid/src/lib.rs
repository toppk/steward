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

/// The stored verification layer: one hash per 1 MiB (64 blocks) of file.
/// Any piece layer for a power-of-two piece size of 1 MiB or more derives
/// from it without reading the file again.
pub const LAYER_PIECE: u64 = 1 << 20;
const LAYER_BLOCKS: usize = (LAYER_PIECE as usize) / BLOCK_SIZE;

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
    pub fn finish(self) -> Option<ContentId> {
        self.finish_with_layer().map(|h| h.id)
    }

    /// The content id plus the 1 MiB verification layer (empty for files
    /// of 1 MiB or less, which BEP 52 gives no piece layer).
    pub fn finish_with_layer(mut self) -> Option<Hashed> {
        if !self.partial.is_empty() {
            self.leaves.push(sha(&self.partial));
        }
        if self.leaves.is_empty() {
            return None;
        }
        let layer = if self.size > LAYER_PIECE {
            self.leaves
                .chunks(LAYER_BLOCKS)
                .map(|c| subtree_root(c, LAYER_BLOCKS))
                .collect()
        } else {
            Vec::new()
        };
        Some(Hashed {
            id: ContentId {
                root: merkle_root(&self.leaves),
                size: self.size,
            },
            layer,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hashed {
    pub id: ContentId,
    /// Hashes of consecutive 1 MiB pieces (see [`LAYER_PIECE`]).
    pub layer: Vec<Hash>,
}

/// Hash of a subtree of `2^height` zero leaves: BEP 52's padding.
pub fn pad_hash(height: u32) -> Hash {
    (0..height).fold([0u8; 32], |h, _| join(&h, &h))
}

/// Root of `leaves` padded with zero leaves to `width` (a power of two).
fn subtree_root(leaves: &[Hash], width: usize) -> Hash {
    let mut layer = leaves.to_vec();
    let mut height = 0;
    while (1 << height) < width {
        if layer.len() % 2 == 1 {
            layer.push(pad_hash(height));
        }
        layer = layer.chunks_exact(2).map(|p| join(&p[0], &p[1])).collect();
        height += 1;
    }
    layer[0]
}

/// The BEP 52 piece layer for `piece_size` (a power of two, at least
/// [`LAYER_PIECE`]) derived from the stored 1 MiB layer of a file of
/// `size` bytes. `None` for an invalid piece size; empty when the file is no
/// bigger than one piece, which BEP 52 gives no piece layer.
pub fn piece_layer(layer: &[Hash], size: u64, piece_size: u64) -> Option<Vec<Hash>> {
    if !piece_size.is_power_of_two() || piece_size < LAYER_PIECE {
        return None;
    }
    if size <= piece_size {
        return Some(Vec::new());
    }
    let mut out = layer.to_vec();
    // Height of a 1 MiB node counted in 16 KiB leaves.
    let mut height = LAYER_BLOCKS.trailing_zeros();
    let mut piece = LAYER_PIECE;
    while piece < piece_size {
        if out.len() % 2 == 1 {
            out.push(pad_hash(height));
        }
        out = out.chunks_exact(2).map(|p| join(&p[0], &p[1])).collect();
        height += 1;
        piece *= 2;
    }
    Some(out)
}

/// The file's root rebuilt from a piece layer, to check a stored layer.
pub fn root_from_layer(layer: &[Hash], piece_size: u64) -> Option<Hash> {
    if layer.is_empty() || !piece_size.is_power_of_two() {
        return None;
    }
    let mut out = layer.to_vec();
    let mut height = (piece_size / BLOCK_SIZE as u64).trailing_zeros();
    while out.len() > 1 {
        if out.len() % 2 == 1 {
            out.push(pad_hash(height));
        }
        out = out.chunks_exact(2).map(|p| join(&p[0], &p[1])).collect();
        height += 1;
    }
    Some(out[0])
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

pub fn hash_reader(r: impl Read) -> io::Result<Option<ContentId>> {
    Ok(hash_reader_with_layer(r)?.map(|h| h.id))
}

pub fn hash_reader_with_layer(mut r: impl Read) -> io::Result<Option<Hashed>> {
    let mut hasher = Hasher::new();
    let mut buf = vec![0u8; 64 * BLOCK_SIZE];
    loop {
        match r.read(&mut buf) {
            Ok(0) => return Ok(hasher.finish_with_layer()),
            Ok(n) => hasher.update(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

pub fn hash_file(path: &Path) -> io::Result<Option<Hashed>> {
    hash_reader_with_layer(File::open(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cid(data: &[u8]) -> Option<ContentId> {
        hash_reader(data).unwrap()
    }

    fn data(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 + i / 7) as u8).collect()
    }

    #[test]
    fn layer_rebuilds_the_root() {
        const MIB: usize = 1 << 20;
        for len in [MIB + 1, 3 * MIB, 5 * MIB + 123, 64 * MIB] {
            let h = hash_reader_with_layer(&data(len)[..]).unwrap().unwrap();
            assert_eq!(h.layer.len(), len.div_ceil(MIB), "{len}");
            assert_eq!(
                root_from_layer(&h.layer, LAYER_PIECE),
                Some(h.id.root),
                "{len}"
            );
        }
    }

    #[test]
    fn small_files_have_no_layer() {
        for len in [1, BLOCK_SIZE, 1 << 20] {
            assert!(
                hash_reader_with_layer(&data(len)[..])
                    .unwrap()
                    .unwrap()
                    .layer
                    .is_empty()
            );
        }
    }

    #[test]
    fn derived_layers_match_direct_computation() {
        let len = 9 * (1 << 20) + 5;
        let d = data(len);
        let h = hash_reader_with_layer(&d[..]).unwrap().unwrap();
        let leaves: Vec<Hash> = d.chunks(BLOCK_SIZE).map(sha).collect();
        for piece in [1u64 << 21, 1 << 22] {
            let per = (piece / BLOCK_SIZE as u64) as usize;
            let direct: Vec<Hash> = leaves.chunks(per).map(|c| subtree_root(c, per)).collect();
            assert_eq!(
                piece_layer(&h.layer, len as u64, piece),
                Some(direct),
                "{piece}"
            );
            let derived = piece_layer(&h.layer, len as u64, piece).unwrap();
            assert_eq!(root_from_layer(&derived, piece), Some(h.id.root));
        }
        assert_eq!(
            piece_layer(&h.layer, len as u64, 1 << 24),
            Some(vec![]),
            "file < piece"
        );
        assert_eq!(
            piece_layer(&h.layer, len as u64, 1 << 19),
            None,
            "below 1 MiB"
        );
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
