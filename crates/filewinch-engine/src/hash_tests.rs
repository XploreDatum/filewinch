//! Expected values come from `tests/data/hash_vectors.py` (an independent Python port of
//! Microsoft's C# QuickXorHash reference, Dropbox's published content_hash recipe, and hashlib).

use super::*;
use filewinch_core::{Caps, Entry, ErrorKind, WriteSink};

/// Same deterministic bytes as `pattern()` in hash_vectors.py.
fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i * 31 + i / 7) % 251) as u8).collect()
}

fn hex_of(algo: HashAlgo, data: &[u8], piece: usize) -> String {
    let mut h = Hasher::new(algo);
    if data.is_empty() {
        h.update(data);
    }
    for c in data.chunks(piece) {
        h.update(c);
    }
    let c = h.finish();
    assert_eq!(c.algo, algo);
    c.hex
}

const MIB4_PLUS_1: usize = 4 * 1024 * 1024 + 1;

#[test]
fn quickxor_matches_reference() {
    let q = HashAlgo::QuickXor;
    assert_eq!(hex_of(q, b"", 1), "0000000000000000000000000000000000000000");
    assert_eq!(hex_of(q, b"Hello, World!", 3), "4828031bd8f0061680e00a6f9d031bc810020000");
    assert_eq!(hex_of(q, &pattern(1000), 1000), "159f5ba6b9d90c1f6c4e88dba4aa152d09f30a16");
    // Odd piece sizes must not change the result (the shift carries across update calls).
    assert_eq!(hex_of(q, &pattern(1000), 7), "159f5ba6b9d90c1f6c4e88dba4aa152d09f30a16");
    assert_eq!(hex_of(q, &pattern(MIB4_PLUS_1), 65_537), "4dbb4d461c47005d3793b65ebcb452b6c7f1ab6a");
}

#[test]
fn dropbox_content_hash_matches_reference() {
    let d = HashAlgo::DropboxContent;
    // No blocks: SHA-256 of nothing.
    assert_eq!(hex_of(d, b"", 1), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    assert_eq!(hex_of(d, b"Hello, World!", 5), "042a7d64a581ef2ee983f21058801cc35663b705e6c55f62fa8e0f18ecc70989");
    // A block boundary plus one byte, fed in pieces that straddle the boundary.
    let big = pattern(MIB4_PLUS_1);
    let want = "6e2124f0fdaa3bea6d1dcc20a318846a247ac2147ff067c2d19813ae79253e41";
    assert_eq!(hex_of(d, &big, 1_000_003), want);
    assert_eq!(hex_of(d, &big, MIB4_PLUS_1), want);
}

#[test]
fn standard_digests_match_reference() {
    let data = pattern(1000);
    assert_eq!(hex_of(HashAlgo::Md5, &data, 333), "f64ddef742eadbd7632df6554fe3b9ff");
    assert_eq!(hex_of(HashAlgo::Sha1, &data, 333), "163dab310e500648b02069493c961b0d952e84e9");
    assert_eq!(
        hex_of(HashAlgo::Sha256, &data, 333),
        "867506a6e8cdbc30b97cb4a6740beb06916ba02da0208d97c1e82a27d60dca2f"
    );
    assert_eq!(hex_of(HashAlgo::Md5, b"", 1), "d41d8cd98f00b204e9800998ecf8427e");
}

#[test]
fn cost_prefers_standard_digests() {
    use HashAlgo::*;
    let mut algos = vec![QuickXor, DropboxContent, Sha256, Sha1, Md5];
    algos.sort_by_key(|a| cost(*a));
    assert_eq!(algos, vec![Md5, Sha1, Sha256, DropboxContent, QuickXor]);
}

/// A remote whose single file reads `data`, then fails with `fail` (if any) at the end.
struct OneFile {
    data: Vec<u8>,
    open_error: Option<ErrorKind>,
    fail: Option<ErrorKind>,
}

struct Failing {
    data: io::Cursor<Vec<u8>>,
    fail: Option<ErrorKind>,
    interrupted_once: bool,
}

impl Read for Failing {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.interrupted_once {
            self.interrupted_once = true;
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        let n = self.data.read(buf)?;
        match (n, self.fail) {
            (0, Some(kind)) => Err(io::Error::other(Error::new(kind, "The connection dropped."))),
            _ => Ok(n),
        }
    }
}

impl Remote for OneFile {
    fn caps(&self) -> Caps {
        Caps::default()
    }
    fn home(&self) -> Result<RPath> {
        Ok(RPath::root())
    }
    fn list(&self, _: &RPath) -> Result<Vec<Entry>> {
        Ok(Vec::new())
    }
    fn stat(&self, p: &RPath) -> Result<Entry> {
        Err(Error::new(ErrorKind::NotFound, format!("{p}")))
    }
    fn read(&self, _: &RPath, _: u64) -> Result<Box<dyn Read + Send>> {
        if let Some(kind) = self.open_error {
            return Err(Error::new(kind, "You don't have permission."));
        }
        Ok(Box::new(Failing { data: io::Cursor::new(self.data.clone()), fail: self.fail, interrupted_once: false }))
    }
    fn write(&self, _: &RPath, _: u64, _: Option<u64>) -> Result<Box<dyn WriteSink>> {
        Err(Error::unsupported("write"))
    }
    fn mkdir(&self, _: &RPath) -> Result<()> {
        Err(Error::unsupported("mkdir"))
    }
    fn rename(&self, _: &RPath, _: &RPath) -> Result<()> {
        Err(Error::unsupported("rename"))
    }
    fn remove_file(&self, _: &RPath) -> Result<()> {
        Err(Error::unsupported("remove"))
    }
    fn remove_dir(&self, _: &RPath) -> Result<()> {
        Err(Error::unsupported("remove"))
    }
}

#[test]
fn hash_file_streams_and_retries_interrupted_reads() {
    let r = OneFile { data: pattern(MIB4_PLUS_1), open_error: None, fail: None };
    let c = hash_file(&r, &RPath::new("/f"), HashAlgo::Sha256, &mut |_| Ok(())).unwrap();
    assert_eq!(c.hex, "cd2bb39479b46f081d1aa3df046608fad97d9087fa72be14b55a0bc1030c929a");
}

#[test]
fn hash_file_keeps_the_error_kind_and_names_the_file() {
    let r = OneFile { data: b"abc".to_vec(), open_error: None, fail: Some(ErrorKind::Network) };
    let e = hash_file(&r, &RPath::new("/dir/f.txt"), HashAlgo::Md5, &mut |_| Ok(())).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Network);
    assert!(e.message.contains("/dir/f.txt") && e.message.contains("The connection dropped."), "{}", e.message);

    let r = OneFile { data: Vec::new(), open_error: Some(ErrorKind::PermissionDenied), fail: None };
    let e = hash_file(&r, &RPath::new("/g"), HashAlgo::Sha1, &mut |_| Ok(())).unwrap_err();
    assert_eq!(e.kind, ErrorKind::PermissionDenied);
    assert!(e.message.starts_with("Couldn't read /g to compare checksums"), "{}", e.message);
}
