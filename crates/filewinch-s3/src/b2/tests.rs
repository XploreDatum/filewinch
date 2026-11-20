//! B2 against the in-memory fake in `fake.rs`.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use filewinch_core::{Auth, ErrorKind, Protocol, RPath, Remote, SecretRef, Site};
use filewinch_http::Http;

use super::fake::{self, State};
use super::upload::plan;
use super::*;
use crate::mock::Mock;

fn site(mock: &Mock) -> Site {
    let mut s = Site::new("b2", "b2", Protocol::B2);
    s.host = mock.url();
    s.user = "keyid".into();
    s.auth = Auth::Password { secret: SecretRef("k".into()) };
    s
}

fn server(tweak: impl FnOnce(&mut State)) -> (Mock, Arc<Mutex<State>>) {
    let mut st = State { key: ("keyid".into(), "appkey".into()), ..State::default() };
    st.buckets.insert("photos".into(), "bk-photos".into());
    st.buckets.insert("docs".into(), "bk-docs".into());
    tweak(&mut st);
    fake::start(st)
}

fn connect(mock: &Mock, limits: Limits) -> filewinch_core::Result<B2Remote> {
    B2Remote::open(&site(mock), &|_| Ok(Some("appkey".into())), Http::new(), limits)
}

fn names(entries: &[filewinch_core::Entry]) -> Vec<(String, bool)> {
    entries.iter().map(|e| (e.name.clone(), e.is_dir())).collect()
}

fn read_all(r: &B2Remote, path: &str, offset: u64) -> Vec<u8> {
    let mut v = Vec::new();
    r.read(&RPath::new(path), offset).unwrap().read_to_end(&mut v).unwrap();
    v
}

#[test]
fn bad_key_is_auth() {
    let (mock, _) = server(|_| {});
    let e = B2Remote::open(&site(&mock), &|_| Ok(Some("wrong".into())), Http::new(), Limits::default()).err().unwrap();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert_eq!(e.message, "Backblaze B2 rejected the key ID or application key.");
}

#[test]
fn lists_buckets_files_and_folders_with_paging() {
    let (mock, st) = server(|s| {
        s.page_size = 3;
        s.put("bk-photos", "a.jpg", b"aaa");
        s.put("bk-photos", "b.jpg", b"b");
        s.put("bk-photos", "trip/x.jpg", b"x");
        s.put("bk-photos", "trip/y.jpg", b"y");
        s.put("bk-photos", "u/.bzEmpty", b"");
        s.put("bk-photos", "z.txt", b"z");
    });
    let r = connect(&mock, Limits::default()).unwrap();
    assert_eq!(names(&r.list(&RPath::root()).unwrap()), [("docs".into(), true), ("photos".into(), true)]);
    let list = r.list(&RPath::new("/photos")).unwrap();
    assert_eq!(
        names(&list),
        [("a.jpg".into(), false), ("b.jpg".into(), false), ("trip".into(), true), ("u".into(), true), ("z.txt".into(), false)]
    );
    assert_eq!(list[0].size, Some(3));
    assert!(list[0].modified.is_some());
    // The empty folder's marker is hidden.
    assert!(r.list(&RPath::new("/photos/u")).unwrap().is_empty());
    assert!(r.stat(&RPath::new("/photos/u")).unwrap().is_dir());
    assert!(r.stat(&RPath::new("/photos/trip")).unwrap().is_dir());
    assert_eq!(r.stat(&RPath::new("/photos/a.jpg")).unwrap().size, Some(3));
    assert_eq!(r.stat(&RPath::new("/photos/nope")).unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(r.list(&RPath::new("/photos/nope")).unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(r.list(&RPath::new("/nobucket")).unwrap_err().kind, ErrorKind::NotFound);
    // Paged: page size 3 needed several calls.
    let pages = mock.seen().iter().filter(|s| s.path().ends_with("b2_list_file_names")).count();
    assert!(pages >= 3, "{pages}");
    drop(st);
}

#[test]
fn small_upload_streams_with_sha1_at_end() {
    let (mock, st) = server(|_| {});
    let r = connect(&mock, Limits::default()).unwrap();
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let mut sink = r.write(&RPath::new("/docs/a b/report.pdf"), 0, Some(data.len() as u64)).unwrap();
    for c in data.chunks(10_007) {
        sink.write_all(c).unwrap();
    }
    sink.finish().unwrap();
    let up = mock.seen().into_iter().find(|s| s.path().starts_with("/upload/")).unwrap();
    assert_eq!(up.header("x-bz-content-sha1"), Some("hex_digits_at_end"));
    assert_eq!(up.header("content-length"), Some((data.len() + 40).to_string().as_str()));
    assert_eq!(up.header("x-bz-file-name"), Some("a%20b/report.pdf"));
    assert_eq!(st.lock().unwrap().latest("bk-docs", "a b/report.pdf").unwrap().data, data);
    // Second upload reuses the pooled upload URL.
    let mut sink = r.write(&RPath::new("/docs/2.txt"), 0, Some(2)).unwrap();
    sink.write_all(b"hi").unwrap();
    sink.finish().unwrap();
    let urls = mock.seen().iter().filter(|s| s.path().ends_with("b2_get_upload_url")).count();
    assert_eq!(urls, 1);
    // Unknown size, small: buffered single upload with its SHA-1.
    let mut sink = r.write(&RPath::new("/docs/3.txt"), 0, None).unwrap();
    sink.write_all(b"abc").unwrap();
    sink.finish().unwrap();
    assert_eq!(read_all(&r, "/docs/3.txt", 0), b"abc");
}

#[test]
fn large_file_flow_known_and_unknown_size() {
    let (mock, st) = server(|s| {
        s.recommended_part = 5_000_000;
        s.min_part = 5_000_000;
    });
    let r = connect(&mock, Limits::default()).unwrap();
    let data: Vec<u8> = (0..12_345_678u32).map(|i| (i.wrapping_mul(7) >> 3) as u8).collect();
    let mut sink = r.write(&RPath::new("/docs/big.bin"), 0, Some(data.len() as u64)).unwrap();
    for c in data.chunks(1 << 20) {
        sink.write_all(c).unwrap();
    }
    sink.finish().unwrap();
    assert_eq!(st.lock().unwrap().latest("bk-docs", "big.bin").unwrap().data, data);
    let parts: Vec<usize> = mock.seen().iter().filter(|s| s.path().starts_with("/part/")).map(|s| s.body.len()).collect();
    assert_eq!(parts, [5_000_000, 5_000_000, 2_345_678]);

    let mut sink = r.write(&RPath::new("/docs/stream.bin"), 0, None).unwrap();
    sink.write_all(&data).unwrap();
    sink.finish().unwrap();
    assert_eq!(read_all(&r, "/docs/stream.bin", 12_000_000), &data[12_000_000..]);
    // B2 has no verified SHA-1 for a large file ("none"): no checksum, on stat or list.
    assert_eq!(st.lock().unwrap().latest("bk-docs", "big.bin").unwrap().sha1, "none");
    assert!(r.stat(&RPath::new("/docs/big.bin")).unwrap().checksums.is_empty());
    assert!(r.list(&RPath::new("/docs")).unwrap().iter().all(|e| e.checksums.is_empty()));
}

#[test]
fn checksums_only_for_verified_sha1() {
    use filewinch_core::{Checksum, HashAlgo};
    use sha1::{Digest, Sha1};
    let big_sha = hex::encode(Sha1::digest(b"big"));
    let (mock, _st) = server(|s| {
        s.put("bk-docs", "small.txt", b"hello");
        s.put("bk-docs", "none.bin", b"n");
        s.versions.last_mut().unwrap().sha1 = "none".into();
        // A large file whose uploader claimed a SHA-1 B2 never checked.
        s.put("bk-docs", "claimed.bin", b"big");
        let v = s.versions.last_mut().unwrap();
        v.sha1 = format!("unverified:{big_sha}");
        v.info = serde_json::json!({ "large_file_sha1": big_sha });
        s.put("bk-docs", "short.bin", b"s");
        s.versions.last_mut().unwrap().sha1 = "abc123".into();
        s.put("bk-docs", "sub/x", b"x");
    });
    let r = connect(&mock, Limits::default()).unwrap();
    let want = vec![Checksum { algo: HashAlgo::Sha1, hex: hex::encode(Sha1::digest(b"hello")) }];
    let list = r.list(&RPath::new("/docs")).unwrap();
    let sums = |n: &str| list.iter().find(|e| e.name == n).unwrap().checksums.clone();
    assert_eq!(sums("small.txt"), want);
    for n in ["none.bin", "claimed.bin", "short.bin", "sub"] {
        assert!(sums(n).is_empty(), "{n}: {:?}", sums(n));
    }
    assert_eq!(r.stat(&RPath::new("/docs/small.txt")).unwrap().checksums, want);
    for n in ["/docs/none.bin", "/docs/claimed.bin", "/docs/short.bin", "/docs/sub"] {
        assert!(r.stat(&RPath::new(n)).unwrap().checksums.is_empty(), "{n}");
    }
}

#[test]
fn dropped_large_upload_is_cancelled() {
    let (mock, st) = server(|s| {
        s.recommended_part = 5_000_000;
        s.min_part = 5_000_000;
    });
    let r = connect(&mock, Limits::default()).unwrap();
    let mut sink = r.write(&RPath::new("/docs/big.bin"), 0, Some(20_000_000)).unwrap();
    sink.write_all(&vec![1u8; 6_000_000]).unwrap();
    drop(sink);
    for _ in 0..100 {
        if !st.lock().unwrap().cancelled.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let s = st.lock().unwrap();
    assert_eq!(s.cancelled.len(), 1);
    assert!(s.latest("bk-docs", "big.bin").is_none());
    drop(mock);
}

#[test]
fn ranged_download_and_resume_at_end() {
    let (mock, _) = server(|s| s.put("bk-docs", "f.txt", b"hello world"));
    let r = connect(&mock, Limits::default()).unwrap();
    assert_eq!(read_all(&r, "/docs/f.txt", 0), b"hello world");
    assert_eq!(read_all(&r, "/docs/f.txt", 6), b"world");
    assert!(read_all(&r, "/docs/f.txt", 11).is_empty());
    let range = mock.seen().into_iter().find(|s| s.header("range").is_some()).unwrap();
    assert_eq!(range.header("range"), Some("bytes=6-"));
    assert_eq!(r.read(&RPath::new("/docs/missing"), 0).err().unwrap().kind, ErrorKind::NotFound);
}

#[test]
fn delete_removes_every_version() {
    let (mock, st) = server(|s| {
        s.put("bk-docs", "f.txt", b"v1");
        s.put("bk-docs", "f.txt", b"v2");
        s.put("bk-docs", "f.txt.bak", b"other");
    });
    let r = connect(&mock, Limits::default()).unwrap();
    r.remove_file(&RPath::new("/docs/f.txt")).unwrap();
    {
        let s = st.lock().unwrap();
        assert!(s.latest("bk-docs", "f.txt").is_none());
        assert!(s.latest("bk-docs", "f.txt.bak").is_some(), "only the exact name");
    }
    assert_eq!(r.remove_file(&RPath::new("/docs/f.txt")).unwrap_err().kind, ErrorKind::NotFound);
    let deletes = mock.seen().iter().filter(|s| s.path().ends_with("b2_delete_file_version")).count();
    assert_eq!(deletes, 2);
}

#[test]
fn folders_mkdir_and_remove() {
    let (mock, st) = server(|s| s.put("bk-docs", "full/x", b"x"));
    let r = connect(&mock, Limits::default()).unwrap();
    r.mkdir(&RPath::new("/docs/new")).unwrap();
    assert!(st.lock().unwrap().latest("bk-docs", "new/.bzEmpty").is_some());
    assert!(r.list(&RPath::new("/docs/new")).unwrap().is_empty());
    assert_eq!(r.remove_dir(&RPath::new("/docs/full")).unwrap_err().kind, ErrorKind::InvalidPath);
    r.remove_dir(&RPath::new("/docs/new")).unwrap();
    assert_eq!(r.stat(&RPath::new("/docs/new")).unwrap_err().kind, ErrorKind::NotFound);
    // Buckets.
    r.mkdir(&RPath::new("/fresh")).unwrap();
    assert_eq!(r.mkdir(&RPath::new("/fresh")).unwrap_err().kind, ErrorKind::AlreadyExists);
    assert!(r.stat(&RPath::new("/fresh")).unwrap().is_dir());
    r.remove_dir(&RPath::new("/fresh")).unwrap();
    assert_eq!(r.remove_dir(&RPath::new("/docs")).unwrap_err().kind, ErrorKind::InvalidPath);
}

#[test]
fn copy_small_and_large() {
    let (mock, st) = server(|s| {
        s.put("bk-docs", "a.bin", &[7u8; 2500]);
    });
    let limits = Limits { copy_max: 1000, copy_part: 1000, ..Limits::default() };
    let r = connect(&mock, limits).unwrap();
    // 2500 bytes > copy_max: three b2_copy_part calls.
    r.copy(&RPath::new("/docs/a.bin"), &RPath::new("/photos/b.bin")).unwrap();
    assert_eq!(st.lock().unwrap().latest("bk-photos", "b.bin").unwrap().data, vec![7u8; 2500]);
    let parts = mock.seen().iter().filter(|s| s.path().ends_with("b2_copy_part")).count();
    assert_eq!(parts, 3);

    let r = connect(&mock, Limits::default()).unwrap();
    r.copy(&RPath::new("/docs/a.bin"), &RPath::new("/docs/c.bin")).unwrap();
    let copy = mock.seen().into_iter().rev().find(|s| s.path().ends_with("b2_copy_file")).unwrap();
    assert!(copy.json().get("destinationBucketId").is_none(), "same bucket");
    assert_eq!(r.copy(&RPath::new("/docs/nope"), &RPath::new("/docs/x")).unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(r.rename(&RPath::new("/docs/a.bin"), &RPath::new("/docs/x")).unwrap_err().kind, ErrorKind::Unsupported);
}

#[test]
fn expired_token_reauthorizes_once() {
    let (mock, st) = server(|s| s.put("bk-docs", "f.txt", b"x"));
    let r = connect(&mock, Limits::default()).unwrap();
    st.lock().unwrap().expire_next = 1;
    assert_eq!(names(&r.list(&RPath::new("/docs")).unwrap()), [("f.txt".into(), false)]);
    assert_eq!(st.lock().unwrap().authorizations, 2);
    // Downloads re-authorize too.
    st.lock().unwrap().expire_next = 1;
    let mut v = Vec::new();
    r.read(&RPath::new("/docs/f.txt"), 0).unwrap().read_to_end(&mut v).unwrap();
    assert_eq!(v, b"x");
    assert_eq!(st.lock().unwrap().authorizations, 3);

    // Expired again right after re-authorizing: give up with Auth.
    st.lock().unwrap().expire_next = 2;
    let e = r.list(&RPath::new("/docs")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth, "{e:?}");
    assert_eq!(st.lock().unwrap().authorizations, 4);
}

#[test]
fn busy_is_rate_limited_with_retry_after() {
    let (mock, st) = server(|_| {});
    let r = connect(&mock, Limits::default()).unwrap();
    st.lock().unwrap().busy_next = 1;
    let e = r.list(&RPath::new("/docs")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::RateLimited);
    assert_eq!(e.retry_after, Some(Duration::from_secs(5)));
    drop(mock);
}

#[test]
fn restricted_key_opens_its_bucket() {
    let (mock, _) = server(|s| s.restricted = Some(("bk-docs".into(), "docs".into())));
    let r = connect(&mock, Limits::default()).unwrap();
    assert_eq!(r.home().unwrap(), RPath::new("/docs"));
    assert_eq!(names(&r.list(&RPath::root()).unwrap()), [("docs".into(), true)]);
    assert!(!mock.seen().iter().any(|s| s.path().ends_with("b2_list_buckets")));
}

#[test]
fn part_plan() {
    let l = Limits::default();
    // B2's recommended 100 MB is capped to 32 MiB of buffer; 100 MB+ files go large.
    assert_eq!(plan(&l, 100_000_000, 5_000_000, Some(99_000_000)), (32 * 1024 * 1024, false));
    assert_eq!(plan(&l, 100_000_000, 5_000_000, Some(100_000_000)), (32 * 1024 * 1024, true));
    assert_eq!(plan(&l, 100_000_000, 5_000_000, None), (32 * 1024 * 1024, true));
    // 1 TB needs bigger parts to stay within 10 000.
    let (p, large) = plan(&l, 100_000_000, 5_000_000, Some(1_000_000_000_000));
    assert!(large && p * 10_000 >= 1_000_000_000_000);
}

#[test]
fn closed_session_refuses_calls() {
    let (mock, _) = server(|_| {});
    let r = connect(&mock, Limits::default()).unwrap();
    r.close();
    assert_eq!(r.list(&RPath::root()).unwrap_err().kind, ErrorKind::Closed);
}
