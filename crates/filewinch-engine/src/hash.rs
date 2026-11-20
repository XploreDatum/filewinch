//! Content hashing for compare-by-checksum: the algorithms servers report ([`HashAlgo`]),
//! computed over a [`Remote::read`] stream in constant memory.

use filewinch_core::{Checksum, Error, HashAlgo, RPath, Remote, Result};
use sha1::Digest as _;
use std::io::{self, Read};

/// Read buffer for hashing.
const BUF: usize = 256 * 1024;

/// Dropbox hashes each 4 MiB block, then hashes the concatenated block hashes.
const DROPBOX_BLOCK: usize = 4 * 1024 * 1024;

/// Relative cost of computing each algorithm; lower is cheaper. Used to pick which hash to
/// compute when a server offers several.
pub(crate) fn cost(algo: HashAlgo) -> u8 {
    match algo {
        HashAlgo::Md5 => 0,
        HashAlgo::Sha1 => 1,
        HashAlgo::Sha256 => 2,
        HashAlgo::DropboxContent => 3,
        HashAlgo::QuickXor => 4,
    }
}

/// An incremental hasher for any [`HashAlgo`].
pub(crate) enum Hasher {
    Md5(Box<md5::Context>),
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
    Dropbox(DropboxHasher),
    QuickXor(QuickXor),
}

impl Hasher {
    pub(crate) fn new(algo: HashAlgo) -> Hasher {
        match algo {
            HashAlgo::Md5 => Hasher::Md5(Box::new(md5::Context::new())),
            HashAlgo::Sha1 => Hasher::Sha1(sha1::Sha1::new()),
            HashAlgo::Sha256 => Hasher::Sha256(sha2::Sha256::new()),
            HashAlgo::DropboxContent => Hasher::Dropbox(DropboxHasher::default()),
            HashAlgo::QuickXor => Hasher::QuickXor(QuickXor::default()),
        }
    }

    pub(crate) fn update(&mut self, data: &[u8]) {
        match self {
            Hasher::Md5(c) => c.consume(data),
            Hasher::Sha1(h) => h.update(data),
            Hasher::Sha256(h) => h.update(data),
            Hasher::Dropbox(h) => h.update(data),
            Hasher::QuickXor(h) => h.update(data),
        }
    }

    pub(crate) fn finish(self) -> Checksum {
        match self {
            Hasher::Md5(c) => Checksum::from_bytes(HashAlgo::Md5, &c.finalize().0),
            Hasher::Sha1(h) => Checksum::from_bytes(HashAlgo::Sha1, &h.finalize()),
            Hasher::Sha256(h) => Checksum::from_bytes(HashAlgo::Sha256, &h.finalize()),
            Hasher::Dropbox(h) => Checksum::from_bytes(HashAlgo::DropboxContent, &h.finish()),
            Hasher::QuickXor(h) => Checksum::from_bytes(HashAlgo::QuickXor, &h.finish()),
        }
    }
}

/// Dropbox `content_hash`: SHA-256 of the concatenated SHA-256 digests of each 4 MiB block
/// (the last block may be shorter; an empty file has no blocks).
#[derive(Default)]
pub(crate) struct DropboxHasher {
    block: sha2::Sha256,
    in_block: usize,
    overall: sha2::Sha256,
}

impl DropboxHasher {
    pub(crate) fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            let take = (DROPBOX_BLOCK - self.in_block).min(data.len());
            self.block.update(&data[..take]);
            self.in_block += take;
            data = &data[take..];
            if self.in_block == DROPBOX_BLOCK {
                let digest = std::mem::take(&mut self.block).finalize();
                self.overall.update(digest);
                self.in_block = 0;
            }
        }
    }

    pub(crate) fn finish(mut self) -> [u8; 32] {
        if self.in_block > 0 {
            let digest = self.block.finalize();
            self.overall.update(digest);
        }
        self.overall.finalize().into()
    }
}

/// Microsoft's QuickXorHash (OneDrive): byte `i` of the input is XORed into a 160-bit
/// circular register at bit `(11 * i) mod 160` (bits numbered little-endian), then the input
/// length as a little-endian u64 is XORed into the register's last 8 bytes.
#[derive(Default)]
pub(crate) struct QuickXor {
    state: [u8; 20],
    /// Bit position for the next byte.
    shift: usize,
    len: u64,
}

impl QuickXor {
    const WIDTH: usize = 160;
    const SHIFT: usize = 11;

    pub(crate) fn update(&mut self, data: &[u8]) {
        for &b in data {
            let (byte, bit) = (self.shift / 8, self.shift % 8);
            let v = u16::from(b) << bit;
            self.state[byte] ^= v as u8;
            // 160 is a multiple of 8, so a byte that straddles the end wraps to byte 0.
            self.state[(byte + 1) % 20] ^= (v >> 8) as u8;
            self.shift = (self.shift + Self::SHIFT) % Self::WIDTH;
        }
        self.len += data.len() as u64;
    }

    pub(crate) fn finish(self) -> [u8; 20] {
        let mut out = self.state;
        for (i, b) in self.len.to_le_bytes().iter().enumerate() {
            out[12 + i] ^= b;
        }
        out
    }
}

/// Reads `path` from `r` start to end and hashes it with `algo`. Read errors keep the
/// backend's error kind (core unwraps a wrapped [`Error`]) and say which file failed.
/// `on_chunk` gets each chunk's length after it is hashed; an error from it (e.g.
/// cancellation) stops the read and is returned as is.
pub(crate) fn hash_file(
    r: &dyn Remote,
    path: &RPath,
    algo: HashAlgo,
    on_chunk: &mut dyn FnMut(u64) -> Result<()>,
) -> Result<Checksum> {
    let mut reader = r.read(path, 0).map_err(|e| context(e, path))?;
    let mut hasher = Hasher::new(algo);
    let mut buf = vec![0u8; BUF];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
                on_chunk(n as u64)?;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(context(Error::from(e), path)),
        }
    }
    Ok(hasher.finish())
}

fn context(mut e: Error, path: &RPath) -> Error {
    let msg = format!("Couldn't read {path} to compare checksums: {}", e.message);
    e.message = msg;
    e
}

#[cfg(test)]
#[path = "hash_tests.rs"]
mod tests;
