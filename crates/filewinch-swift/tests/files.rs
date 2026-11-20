//! Listing, folders, small uploads, reads (ranged, cut off), copy and delete.

mod mock;

use std::io::{Read, Write};

use filewinch_core::{EntryKind, ErrorKind, RPath, Remote};
use filewinch_swift::Tuning;
use filewinch_testkit::FaultProxy;
use mock::{Mock, PASS, connect};

fn small_pages() -> Tuning {
    Tuning { page_size: 2, ..Tuning::default() }
}

fn names(r: &impl Remote, p: &str) -> Vec<(String, EntryKind)> {
    let mut v: Vec<_> = r.list(&RPath::new(p)).unwrap().into_iter().map(|e| (e.name, e.kind)).collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

fn read_all(r: &impl Remote, p: &str, offset: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    r.read(&RPath::new(p), offset).unwrap().read_to_end(&mut buf).unwrap();
    buf
}

fn upload(r: &impl Remote, p: &str, data: &[u8]) {
    let mut sink = r.write(&RPath::new(p), 0, Some(data.len() as u64)).unwrap();
    sink.write_all(data).unwrap();
    sink.finish().unwrap();
}

#[test]
fn lists_containers_with_paging() {
    let m = Mock::start();
    for c in ["a", "b", "c", "d", "e"] {
        m.put(c, "x", b"1");
    }
    let r = m.connect(small_pages());
    let got = names(&r, "/");
    assert_eq!(got.len(), 5);
    assert!(got.iter().all(|(_, k)| *k == EntryKind::Dir));
    assert!(m.st().log.iter().filter(|l| l.starts_with("GET") && l.contains("format=json")).count() >= 3);
}

#[test]
fn lists_pseudo_folders_with_paging_and_markers() {
    let m = Mock::start();
    for k in ["a.txt", "b.txt", "docs/", "docs/x.txt", "docs/sub/y.txt", "img/1.png", "img/2.png", "z.txt", "docs/z/"] {
        m.put("c", k, b"data");
    }
    // An old-style folder object without the slash.
    {
        let mut st = m.st();
        st.containers.get_mut("c").unwrap().insert(
            "old".into(),
            mock::Obj { data: Vec::new(), content_type: "application/directory".into(), etag: String::new(), manifest: None, dlo: None },
        );
    }
    let r = m.connect(small_pages());
    use EntryKind::*;
    assert_eq!(
        names(&r, "/c"),
        [("a.txt".into(), File), ("b.txt".into(), File), ("docs".into(), Dir), ("img".into(), Dir), ("old".into(), Dir), ("z.txt".into(), File)]
    );
    // The folder's own marker is skipped; a nested marker shows once.
    assert_eq!(names(&r, "/c/docs"), [("sub".into(), Dir), ("x.txt".into(), File), ("z".into(), Dir)]);
    assert_eq!(names(&r, "/c/docs/z"), []);
    assert_eq!(names(&r, "/c/old"), []);
    let f = r.list(&RPath::new("/c")).unwrap().into_iter().find(|e| e.name == "a.txt").unwrap();
    assert_eq!(f.size, Some(4));
    assert!(f.modified.is_some());
    assert_eq!(r.list(&RPath::new("/c/nope")).unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(r.list(&RPath::new("/c/a.txt")).unwrap_err().kind, ErrorKind::InvalidPath);
    assert_eq!(r.list(&RPath::new("/missing")).unwrap_err().kind, ErrorKind::NotFound);
}

#[test]
fn stat_files_folders_containers() {
    let m = Mock::start();
    m.put("c", "dir/f.txt", b"hello");
    let r = m.connect(Tuning::default());
    let f = r.stat(&RPath::new("/c/dir/f.txt")).unwrap();
    assert_eq!((f.kind, f.size, f.name.as_str()), (EntryKind::File, Some(5), "f.txt"));
    assert!(f.modified.is_some());
    assert_eq!(r.stat(&RPath::new("/c/dir")).unwrap().kind, EntryKind::Dir);
    assert_eq!(r.stat(&RPath::new("/c")).unwrap().kind, EntryKind::Dir);
    assert_eq!(r.stat(&RPath::root()).unwrap().kind, EntryKind::Dir);
    assert_eq!(r.stat(&RPath::new("/c/nope")).unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(r.stat(&RPath::new("/nope")).unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(r.stat(&RPath::new("/nope/x")).unwrap_err().kind, ErrorKind::NotFound);
}

#[test]
fn mkdir_containers_and_folders() {
    let m = Mock::start();
    let r = m.connect(Tuning::default());
    r.mkdir(&RPath::new("/new")).unwrap();
    assert_eq!(r.mkdir(&RPath::new("/new")).unwrap_err().kind, ErrorKind::AlreadyExists);
    r.mkdir(&RPath::new("/new/folder one")).unwrap();
    assert!(m.has("new", "folder one/"));
    assert_eq!(m.st().containers["new"]["folder one/"].content_type, "application/directory");
    assert_eq!(names(&r, "/new"), [("folder one".to_string(), EntryKind::Dir)]);
    assert_eq!(r.mkdir(&RPath::new("/new/folder one")).unwrap_err().kind, ErrorKind::AlreadyExists);
    assert_eq!(r.mkdir(&RPath::new("/missing/f")).unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(r.mkdir(&RPath::root()).unwrap_err().kind, ErrorKind::InvalidPath);
}

#[test]
fn small_upload_is_byte_equal_with_odd_names() {
    let m = Mock::start();
    m.put("c", "seed", b"");
    let r = m.connect(Tuning::default());
    let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
    upload(&r, "/c/a dir/ünï cødé?#%.bin", &data);
    assert!(m.has("c", "a dir/ünï cødé?#%.bin"));
    assert_eq!(read_all(&r, "/c/a dir/ünï cødé?#%.bin", 0), data);
    upload(&r, "/c/empty", b"");
    assert_eq!(read_all(&r, "/c/empty", 0), b"");
    assert_eq!(r.write(&RPath::new("/c"), 0, Some(1)).err().unwrap().kind, ErrorKind::InvalidPath);
    assert_eq!(r.write(&RPath::new("/c/x"), 5, Some(10)).err().unwrap().kind, ErrorKind::Unsupported);
}

#[test]
fn upload_size_mismatch_and_drop_leave_nothing() {
    let m = Mock::start();
    m.put("c", "seed", b"");
    let r = m.connect(Tuning::default());
    // Short: finish before the declared size.
    let mut s = r.write(&RPath::new("/c/short"), 0, Some(10)).unwrap();
    s.write_all(b"12345").unwrap();
    assert!(s.finish().is_err());
    // Long: more than declared.
    let mut s = r.write(&RPath::new("/c/long"), 0, Some(3)).unwrap();
    assert!(s.write_all(b"12345").is_err());
    drop(s);
    // Every byte written, then dropped without finish: not committed.
    let mut s = r.write(&RPath::new("/c/dropped"), 0, Some(5)).unwrap();
    s.write_all(b"12345").unwrap();
    s.flush().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    drop(s);
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(m.keys("c"), ["seed"]);
}

#[test]
fn damaged_upload_is_reported_and_removed() {
    let m = Mock::start();
    m.put("c", "seed", b"");
    let r = m.connect(Tuning::default());
    m.st().bad_etag = true;
    let mut s = r.write(&RPath::new("/c/f"), 0, Some(4)).unwrap();
    s.write_all(b"abcd").unwrap();
    let e = s.finish().unwrap_err();
    assert_eq!(e.kind, ErrorKind::Network);
    assert!(e.message.contains("damaged"), "{e}");
    assert!(!m.has("c", "f"));
}

#[test]
fn upload_quota_is_no_space() {
    let m = Mock::start();
    m.put("c", "seed", b"");
    let r = m.connect(Tuning::default());
    m.st().fail.push_back((413, vec![], "Upload exceeds quota.".into()));
    let mut s = r.write(&RPath::new("/c/f"), 0, Some(4)).unwrap();
    let w = s.write_all(b"abcd").and_then(|_| s.flush());
    let e = match w {
        Err(e) => filewinch_http::from_io(e),
        Ok(()) => s.finish().unwrap_err(),
    };
    assert_eq!(e.kind, ErrorKind::NoSpace);
    assert!(e.message.contains("quota"), "{e}");
}

#[test]
fn ranged_reads() {
    let m = Mock::start();
    m.put("c", "f", b"0123456789");
    let r = m.connect(Tuning::default());
    assert_eq!(read_all(&r, "/c/f", 4), b"456789");
    assert_eq!(read_all(&r, "/c/f", 10), b"");
    assert_eq!(r.read(&RPath::new("/c/f"), 11).err().unwrap().kind, ErrorKind::InvalidPath);
    assert_eq!(r.read(&RPath::new("/c/nope"), 0).err().unwrap().kind, ErrorKind::NotFound);
    assert_eq!(r.read(&RPath::new("/c"), 0).err().unwrap().kind, ErrorKind::InvalidPath);
}

#[test]
fn cut_mid_download_is_network() {
    let m = Mock::start();
    let data = vec![7u8; 2 * 1024 * 1024];
    m.put("c", "big", &data);
    let proxy = FaultProxy::start(m.addr);
    let mut site = m.site("/v3", "test:tester");
    site.host = format!("http://127.0.0.1:{}/v3", proxy.port);
    let r = connect(&site, PASS, Tuning::default()).unwrap();
    let mut reader = r.read(&RPath::new("/c/big"), 0).unwrap();
    proxy.cut_after(256 * 1024);
    let mut buf = Vec::new();
    let e = reader.read_to_end(&mut buf).unwrap_err();
    assert!(buf.len() < data.len());
    let e = filewinch_http::from_io(e);
    assert_eq!(e.kind, ErrorKind::Network, "{e}");
    // Resume from where it stopped.
    let rest = read_all(&r, "/c/big", buf.len() as u64);
    assert_eq!([buf, rest].concat(), data);
}

#[test]
fn copy_delete_rename() {
    let m = Mock::start();
    m.put("c", "a/f.txt", b"hello");
    m.put("d", "seed", b"");
    let r = m.connect(Tuning::default());
    r.copy(&RPath::new("/c/a/f.txt"), &RPath::new("/d/b c/g.txt")).unwrap();
    assert_eq!(read_all(&r, "/d/b c/g.txt", 0), b"hello");
    assert_eq!(r.copy(&RPath::new("/c/nope"), &RPath::new("/d/x")).unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(r.copy(&RPath::new("/c/a/f.txt"), &RPath::new("/d")).unwrap_err().kind, ErrorKind::InvalidPath);
    assert_eq!(r.rename(&RPath::new("/c/a/f.txt"), &RPath::new("/c/g")).unwrap_err().kind, ErrorKind::Unsupported);
    r.remove_file(&RPath::new("/c/a/f.txt")).unwrap();
    assert!(!m.has("c", "a/f.txt"));
    assert_eq!(r.remove_file(&RPath::new("/c/a/f.txt")).unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(r.remove_file(&RPath::new("/c")).unwrap_err().kind, ErrorKind::InvalidPath);
}

#[test]
fn remove_dir_rules() {
    let m = Mock::start();
    m.put("c", "full/f", b"1");
    m.put("c", "empty/", b"");
    m.put("c", "withmarker/", b"");
    m.put("c", "withmarker/f", b"1");
    let r = m.connect(Tuning::default());
    assert_eq!(r.remove_dir(&RPath::new("/c")).unwrap_err().kind, ErrorKind::InvalidPath);
    assert_eq!(r.remove_dir(&RPath::new("/c/full")).unwrap_err().kind, ErrorKind::InvalidPath);
    assert_eq!(r.remove_dir(&RPath::new("/c/withmarker")).unwrap_err().kind, ErrorKind::InvalidPath);
    r.remove_dir(&RPath::new("/c/empty")).unwrap();
    assert!(!m.has("c", "empty/"));
    assert_eq!(r.remove_dir(&RPath::new("/c/empty")).unwrap_err().kind, ErrorKind::NotFound);
    r.mkdir(&RPath::new("/gone")).unwrap();
    r.remove_dir(&RPath::new("/gone")).unwrap();
    assert_eq!(r.remove_dir(&RPath::new("/gone")).unwrap_err().kind, ErrorKind::NotFound);
}

#[test]
fn token_expiry_during_streamed_upload_signs_in_and_asks_for_retry() {
    let m = Mock::start();
    m.put("c", "seed", b"");
    let r = m.connect(Tuning::default());
    m.st().tokens.clear();
    let before = m.st().auths;
    let mut s = r.write(&RPath::new("/c/f"), 0, Some(4)).unwrap();
    let e = match s.write_all(b"abcd").and_then(|_| s.flush()) {
        Err(e) => filewinch_http::from_io(e),
        Ok(()) => s.finish().unwrap_err(),
    };
    assert_eq!(e.kind, ErrorKind::Network, "{e}");
    assert!(e.kind.is_transient());
    assert_eq!(m.st().auths, before + 1);
    assert!(!m.has("c", "f"));
    // The engine's retry then goes through with the new token.
    upload(&r, "/c/f", b"abcd");
    assert_eq!(read_all(&r, "/c/f", 0), b"abcd");

    // If signing in again fails too, that failure is returned (not a retryable one).
    m.st().tokens.clear();
    m.st().disabled.insert("v3");
    let mut s = r.write(&RPath::new("/c/g"), 0, Some(4)).unwrap();
    let e = match s.write_all(b"abcd").and_then(|_| s.flush()) {
        Err(e) => filewinch_http::from_io(e),
        Ok(()) => s.finish().unwrap_err(),
    };
    assert_ne!(e.kind, ErrorKind::Network, "{e}");
}

#[test]
fn plain_objects_report_md5_on_list_and_stat() {
    use filewinch_core::{Checksum, HashAlgo};
    let m = Mock::start();
    m.put("c", "hello.txt", b"hello");
    m.put("c", "empty", b"");
    m.put("c", "dir/x", b"x");
    let r = m.connect(Tuning::default());
    let md5 = |d: &[u8]| vec![Checksum { algo: HashAlgo::Md5, hex: format!("{:x}", md5::compute(d)) }];
    let list = r.list(&RPath::new("/c")).unwrap();
    let sums = |n: &str| list.iter().find(|e| e.name == n).unwrap().checksums.clone();
    assert_eq!(sums("hello.txt"), md5(b"hello"));
    // A 0-byte listing row could be a DLO manifest: no checksum from the listing...
    assert!(sums("empty").is_empty());
    assert!(sums("dir").is_empty());
    assert_eq!(r.stat(&RPath::new("/c/hello.txt")).unwrap().checksums, md5(b"hello"));
    // ...but HEAD says it isn't one.
    assert_eq!(r.stat(&RPath::new("/c/empty")).unwrap().checksums, md5(b""));
    assert!(r.stat(&RPath::new("/c/dir")).unwrap().checksums.is_empty());
}
