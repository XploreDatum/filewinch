//! The folder tree the round trip uploads, and comparisons of trees on disk and on a remote.

use std::collections::BTreeMap;
use std::path::Path;

use filewinch_core::{EntryKind, RPath, Remote};

pub const UNICODE_NAME: &str = "Ünïcødé ✓ 日本.txt";
pub const BIG_NAME: &str = "big.bin";
pub const BIG_SIZE: usize = 20 * 1024 * 1024;

/// Deterministic pseudo-random bytes (xorshift64*), so a short read or a reordered chunk
/// can't compare equal the way a repeating pattern might.
pub fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        out.extend_from_slice(&x.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Writes the fixture into `root` (created): a 20 MB file, a unicode name, an empty file and,
/// when `nested`, nested folders and an empty folder (else every file sits in `root`).
pub fn write_fixture(root: &Path, nested: bool) {
    let w = |rel: &str, bytes: &[u8]| {
        let rel = if nested { rel } else { rel.rsplit('/').next().expect("name") };
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("create fixture folder");
        std::fs::write(&p, bytes).expect("write fixture file");
    };
    w("readme.md", b"# Filewinch e2e\n\nA small text file.\n");
    w("docs/guide.txt", &pseudo_random(70_001, 7));
    w("docs/nested/deeper/notes.txt", b"three levels down\n");
    w(&format!("media/{BIG_NAME}"), &pseudo_random(BIG_SIZE, 42));
    w(UNICODE_NAME, "unicode content: ✓ 日本語 Ünïcødé\n".as_bytes());
    w("empty.txt", b"");
    if nested {
        std::fs::create_dir_all(root.join("empty-folder")).expect("create empty folder");
    }
}

/// A nested folder made on the server's own disk (not uploaded), for downloads of folders
/// every server can hold, including `rclone serve s3`.
pub fn write_server_tree(root: &Path) {
    let w = |rel: &str, bytes: &[u8]| {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("create server folder");
        std::fs::write(&p, bytes).expect("write server file");
    };
    w("site/index.html", b"<!doctype html><title>e2e</title>\n");
    w("site/assets/img/photo.jpg", &pseudo_random(300_017, 11));
    w("site/assets/css/style.css", b"body { margin: 0 }\n");
    w(&format!("site/{UNICODE_NAME}"), "server unicode ✓\n".as_bytes());
}

/// What a tree holds: relative path -> None for a folder, Some(bytes) for a file.
pub type Snapshot = BTreeMap<String, Option<Vec<u8>>>;

pub fn snapshot_local(root: &Path) -> Snapshot {
    fn walk(dir: &Path, rel: &str, out: &mut Snapshot) {
        let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())).map(|e| e.expect("dir entry")).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            let rel = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            let ty = e.file_type().expect("file type");
            if ty.is_dir() {
                out.insert(rel.clone(), None);
                walk(&e.path(), &rel, out);
            } else {
                out.insert(rel, Some(std::fs::read(e.path()).expect("read file")));
            }
        }
    }
    let mut out = Snapshot::new();
    walk(root, "", &mut out);
    out
}

/// What a remote folder lists: relative path -> (kind, size). Sizes of folders are ignored.
pub fn list_remote(remote: &dyn Remote, root: &RPath) -> BTreeMap<String, (EntryKind, Option<u64>)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![(root.clone(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        for e in remote.list(&dir).unwrap_or_else(|e| panic!("list {dir}: {e:?}")) {
            let r = if rel.is_empty() { e.name.clone() } else { format!("{rel}/{}", e.name) };
            if e.is_dir() {
                stack.push((e.path.clone(), r.clone()));
                out.insert(r, (EntryKind::Dir, None));
            } else {
                out.insert(r, (e.kind, e.size));
            }
        }
    }
    out
}

/// The listing a remote should show for a local snapshot.
pub fn expected_listing(s: &Snapshot) -> BTreeMap<String, (EntryKind, Option<u64>)> {
    s.iter()
        .map(|(k, v)| (k.clone(), v.as_ref().map_or((EntryKind::Dir, None), |b| (EntryKind::File, Some(b.len() as u64)))))
        .collect()
}

/// Compares two snapshots, naming the first difference rather than dumping 20 MB.
#[track_caller]
pub fn assert_same(what: &str, want: &Snapshot, got: &Snapshot) {
    let names = |s: &Snapshot| s.keys().cloned().collect::<Vec<_>>();
    assert_eq!(names(want), names(got), "{what}: different items");
    for (k, w) in want {
        let g = &got[k];
        match (w, g) {
            (None, None) => {}
            (Some(w), Some(g)) => {
                if w != g {
                    let at = w.iter().zip(g.iter()).position(|(a, b)| a != b).unwrap_or(w.len().min(g.len()));
                    panic!("{what}: {k} differs ({} vs {} bytes, first difference at byte {at})", w.len(), g.len());
                }
            }
            _ => panic!("{what}: {k} is a file on one side and a folder on the other"),
        }
    }
}

/// Reads a whole remote file.
pub fn read_remote(remote: &dyn Remote, path: &RPath) -> Vec<u8> {
    let mut r = remote.read(path, 0).unwrap_or_else(|e| panic!("open {path}: {e:?}"));
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut r, &mut buf).unwrap_or_else(|e| panic!("read {path}: {e:?}"));
    buf
}
