//! Against a real Azurite blob service (skipped with a note when Node isn't available).

mod common;

use std::io::{Read, Write};

use common::{Azurite, DEV_KEY, connect, connect_paged};
use filewinch_core::{ErrorKind, RPath, Remote};
use filewinch_testkit::FaultProxy;

fn p(s: &str) -> RPath {
    RPath::new(s)
}

/// Deterministic pseudo-random bytes (so a byte shift can't go unnoticed).
fn data(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn put(r: &dyn Remote, path: &str, bytes: &[u8], size: Option<u64>) {
    let mut sink = r.write(&p(path), 0, size).unwrap();
    // Odd-sized writes, like a real copy loop with partial writes.
    for chunk in bytes.chunks(300_001) {
        sink.write_all(chunk).unwrap();
    }
    sink.finish().unwrap();
}

fn get(r: &dyn Remote, path: &str, offset: u64) -> Vec<u8> {
    let mut out = Vec::new();
    r.read(&p(path), offset).unwrap().read_to_end(&mut out).unwrap();
    out
}

fn names(r: &dyn Remote, dir: &str) -> Vec<String> {
    let mut v: Vec<String> = r.list(&p(dir)).unwrap().into_iter().map(|e| e.name).collect();
    v.sort();
    v
}

#[test]
fn containers_folders_files_copy_delete() {
    let Some(az) = Azurite::start() else { return };
    let r = az.remote();
    assert_eq!(r.home().unwrap(), RPath::root());
    let caps = r.caps();
    assert!(caps.resume_read && caps.server_copy && !caps.real_dirs && !caps.server_rename && !caps.resume_write);

    r.mkdir(&p("/box")).unwrap();
    assert!(names(&*r, "/").contains(&"box".to_string()));
    assert_eq!(r.mkdir(&p("/box")).unwrap_err().kind, ErrorKind::AlreadyExists);
    assert_eq!(r.mkdir(&p("/Bad_Name")).unwrap_err().kind, ErrorKind::InvalidPath);
    assert!(r.stat(&p("/box")).unwrap().is_dir());
    assert_eq!(r.list(&p("/box")).unwrap().len(), 0);

    // Folder marker.
    r.mkdir(&p("/box/docs")).unwrap();
    assert_eq!(r.mkdir(&p("/box/docs")).unwrap_err().kind, ErrorKind::AlreadyExists);
    assert_eq!(names(&*r, "/box"), ["docs"]);
    assert!(r.stat(&p("/box/docs")).unwrap().is_dir());
    assert_eq!(r.list(&p("/box/docs")).unwrap(), vec![], "own marker is skipped");

    // Small file (single Put Blob), names that need encoding.
    let small = data(150_000, 1);
    put(&*r, "/box/docs/R&D 100%+ü.bin", &small, Some(small.len() as u64));
    assert_eq!(get(&*r, "/box/docs/R&D 100%+ü.bin", 0), small);
    assert_eq!(get(&*r, "/box/docs/R&D 100%+ü.bin", 1000), &small[1000..]);
    assert_eq!(get(&*r, "/box/docs/R&D 100%+ü.bin", small.len() as u64), Vec::<u8>::new());
    let e = r.stat(&p("/box/docs/R&D 100%+ü.bin")).unwrap();
    assert!(!e.is_dir());
    assert_eq!(e.size, Some(small.len() as u64));
    assert!(e.modified.is_some());
    // Put Blob: the service records the Content-MD5, on Get Properties and in List Blobs.
    let md5 = filewinch_core::Checksum { algo: filewinch_core::HashAlgo::Md5, hex: format!("{:x}", md5::compute(&small)) };
    assert_eq!(e.checksums, std::slice::from_ref(&md5));
    assert_eq!(r.list(&p("/box/docs")).unwrap()[0].checksums, [md5]);
    assert_eq!(names(&*r, "/box/docs"), ["R&D 100%+ü.bin"]);
    assert_eq!(r.mkdir(&p("/box/docs/R&D 100%+ü.bin")).unwrap_err().kind, ErrorKind::AlreadyExists);

    // Implied folders (no marker) show up and stat as folders.
    put(&*r, "/box/a/b/c.txt", b"hello", Some(5));
    assert_eq!(names(&*r, "/box"), ["a", "docs"]);
    assert!(r.stat(&p("/box/a/b")).unwrap().is_dir());

    // Empty file.
    put(&*r, "/box/empty", b"", Some(0));
    assert_eq!(r.stat(&p("/box/empty")).unwrap().size, Some(0));
    assert_eq!(get(&*r, "/box/empty", 0), Vec::<u8>::new());

    // Server-side copy.
    r.copy(&p("/box/docs/R&D 100%+ü.bin"), &p("/box/copy.bin")).unwrap();
    assert_eq!(get(&*r, "/box/copy.bin", 0), small);
    assert_eq!(r.copy(&p("/box/nope"), &p("/box/x")).unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(r.rename(&p("/box/copy.bin"), &p("/box/y")).unwrap_err().kind, ErrorKind::Unsupported);

    // Not found.
    assert_eq!(r.stat(&p("/box/missing")).unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(r.read(&p("/box/missing"), 0).err().unwrap().kind, ErrorKind::NotFound);
    assert_eq!(r.list(&p("/box/missing")).unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(r.list(&p("/box/copy.bin")).unwrap_err().kind, ErrorKind::InvalidPath);
    assert_eq!(r.list(&p("/nocontainer")).unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(r.stat(&p("/nocontainer")).unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(r.remove_file(&p("/box/missing")).unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(r.write(&p("/nocontainer/f"), 0, Some(3)).map(|mut s| {
        s.write_all(b"abc").ok();
        s.finish()
    }).unwrap().unwrap_err().kind, ErrorKind::NotFound);

    // Deletes: folders must be empty.
    assert_eq!(r.remove_dir(&p("/box/docs")).unwrap_err().kind, ErrorKind::InvalidPath);
    assert_eq!(r.remove_dir(&p("/box")).unwrap_err().kind, ErrorKind::InvalidPath);
    r.remove_file(&p("/box/docs/R&D 100%+ü.bin")).unwrap();
    r.remove_dir(&p("/box/docs")).unwrap();
    assert_eq!(r.stat(&p("/box/docs")).unwrap_err().kind, ErrorKind::NotFound);
    for f in ["/box/a/b/c.txt", "/box/empty", "/box/copy.bin"] {
        r.remove_file(&p(f)).unwrap();
    }
    r.remove_dir(&p("/box/a/b")).unwrap(); // implied folder, already gone: fine
    r.remove_dir(&p("/box")).unwrap();
    assert!(!names(&*r, "/").contains(&"box".to_string()));

    r.close();
    assert_eq!(r.list(&p("/")).unwrap_err().kind, ErrorKind::Closed);
}

#[test]
fn large_uploads_use_blocks() {
    let Some(az) = Azurite::start() else { return };
    let r = az.remote();
    r.mkdir(&p("/big")).unwrap();
    // 12.5 MB, size known: two blocks.
    let known = data(12_500_000, 2);
    put(&*r, "/big/known.bin", &known, Some(known.len() as u64));
    assert_eq!(get(&*r, "/big/known.bin", 0), known);
    assert_eq!(get(&*r, "/big/known.bin", 9_000_000), &known[9_000_000..]);
    // Unknown size: blocks too.
    let unknown = data(9_000_000, 3);
    put(&*r, "/big/unknown.bin", &unknown, None);
    assert_eq!(get(&*r, "/big/unknown.bin", 0), unknown);
    // Small but unknown size, and empty with unknown size.
    put(&*r, "/big/tiny.bin", b"tiny", None);
    assert_eq!(get(&*r, "/big/tiny.bin", 0), b"tiny");
    put(&*r, "/big/none.bin", b"", None);
    assert_eq!(r.stat(&p("/big/none.bin")).unwrap().size, Some(0));
}

#[test]
fn size_mismatch_and_dropped_sinks_commit_nothing() {
    let Some(az) = Azurite::start() else { return };
    let r = az.remote();
    r.mkdir(&p("/drop")).unwrap();

    // Single put: every byte written, then dropped.
    let mut s = r.write(&p("/drop/small"), 0, Some(1000)).unwrap();
    s.write_all(&data(1000, 4)).unwrap();
    drop(s);
    // Blocks: a full block staged, then dropped.
    let mut s = r.write(&p("/drop/large"), 0, Some(20_000_000)).unwrap();
    s.write_all(&data(9_000_000, 5)).unwrap();
    drop(s);
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert_eq!(r.stat(&p("/drop/small")).unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(r.stat(&p("/drop/large")).unwrap_err().kind, ErrorKind::NotFound);

    // Fewer bytes than declared: finish fails, nothing committed.
    let mut s = r.write(&p("/drop/short"), 0, Some(10)).unwrap();
    s.write_all(b"abc").unwrap();
    assert!(s.finish().is_err());
    // More bytes than declared.
    let mut s = r.write(&p("/drop/long"), 0, Some(2)).unwrap();
    assert!(s.write_all(b"abc").is_err());
    assert!(r.list(&p("/drop")).unwrap().is_empty());
}

#[test]
fn wrong_key_is_auth() {
    let Some(az) = Azurite::start() else { return };
    let other = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";
    let e = connect(&az.site(), other).err().expect("must fail");
    assert_eq!(e.kind, ErrorKind::Auth, "{e}");
    assert!(e.message.contains("devstoreaccount1"), "{}", e.message);
    let all = format!("{e} {:?}", e.detail);
    assert!(!all.contains(other) && !all.contains(DEV_KEY), "{all}");
}

#[test]
fn listings_page_with_markers() {
    let Some(az) = Azurite::start() else { return };
    let r = connect_paged(&az.site(), DEV_KEY, 2).unwrap();
    for c in ["pga", "pgb", "pgc", "pgd", "pge"] {
        r.mkdir(&p(&format!("/{c}"))).unwrap();
    }
    assert_eq!(names(&*r, "/"), ["pga", "pgb", "pgc", "pgd", "pge"]);
    r.mkdir(&p("/pga/dir")).unwrap();
    for i in 0..7 {
        put(&*r, &format!("/pga/dir/f{i}"), b"x", Some(1));
    }
    put(&*r, "/pga/dir/sub/inner", b"x", Some(1));
    let mut want: Vec<String> = (0..7).map(|i| format!("f{i}")).collect();
    want.push("sub".into());
    assert_eq!(names(&*r, "/pga/dir"), want);
}

#[test]
fn cut_mid_download_is_network() {
    let Some(az) = Azurite::start() else { return };
    let r = az.remote();
    r.mkdir(&p("/cut")).unwrap();
    let bytes = data(3_000_000, 6);
    put(&*r, "/cut/f.bin", &bytes, Some(bytes.len() as u64));

    let proxy = FaultProxy::start(([127, 0, 0, 1], az.port).into());
    let via = connect(&common::site(proxy.port, common::DEV_ACCOUNT), DEV_KEY).unwrap();
    // Armed before the request: the proxy forwards nothing past the budget (the request is a
    // few hundred bytes of it), so the body can't all slip into socket buffers before the cut.
    proxy.cut_after(1_000_000);
    let mut reader = via.read(&p("/cut/f.bin"), 0).unwrap();
    let mut out = Vec::new();
    let err = reader.read_to_end(&mut out).unwrap_err();
    let core = filewinch_http::from_io(err);
    assert_eq!(core.kind, ErrorKind::Network, "{core}");
    assert!(out.len() < bytes.len());
    // Resume from where it stopped.
    let rest = get(&*via, "/cut/f.bin", out.len() as u64);
    out.extend_from_slice(&rest);
    assert_eq!(out, bytes);
}
