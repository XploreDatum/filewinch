//! Box backend against an in-memory fake of the Box API.

mod mock_box;
mod support;

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

use filewinch_cloud::{BoxConnector, Tuning};
use filewinch_core::{Checksum, Connector, ErrorKind, HashAlgo, Protocol, RPath, Remote};
use mock_box::{FakeBox, PART_SIZE};
use support::{Mock, Reply, bytes};


fn tuning() -> Tuning {
    Tuning {
        simple_max: 100 * 1024,
        chunk: 8 * 1024,
        cache_capacity: 1000,
        cache_ttl: Duration::from_secs(60),
        page_size: 2,
    }
}

struct Fx {
    mock: Mock,
    fake: Arc<FakeBox>,
    remote: Box<dyn Remote>,
    rotated: Arc<support::Recording>,
}

fn setup() -> Fx {
    let (mock, fake) = FakeBox::start();
    let rotated = Arc::new(support::Recording::default());
    let remote = BoxConnector::with_options(support::options(&mock, tuning()))
        .connect(&support::site(Protocol::Box), rotated.clone())
        .unwrap();
    Fx {
        mock,
        fake,
        remote,
        rotated,
    }
}

fn p(s: &str) -> RPath {
    RPath::new(s)
}

fn put(r: &dyn Remote, path: &str, data: &[u8], size: Option<u64>) -> filewinch_core::Result<()> {
    let mut w = r.write(&p(path), 0, size)?;
    for c in data.chunks(7000) {
        // The sink wraps its core Error in the io::Error; unwrap it (see core request C-2).
        w.write_all(c).map_err(|e| {
            match e
                .into_inner()
                .map(|i| i.downcast::<filewinch_core::Error>())
            {
                Some(Ok(inner)) => *inner,
                Some(Err(other)) => filewinch_core::Error::from(std::io::Error::other(other)),
                None => filewinch_core::Error::new(ErrorKind::Other, "write failed"),
            }
        })?;
    }
    w.finish()
}

fn get(r: &dyn Remote, path: &str, offset: u64) -> Vec<u8> {
    let mut out = Vec::new();
    r.read(&p(path), offset)
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    out
}

#[test]
fn refresh_on_401_rotates_and_reports_each_token() {
    let fx = setup();
    assert_eq!(
        fx.rotated.saved(),
        [("oauth.site-1".to_string(), "refresh-1".to_string())]
    );
    fx.remote.list(&RPath::root()).unwrap();
    fx.fake.tokens.expire_all();
    fx.remote.list(&RPath::root()).unwrap();
    assert_eq!(fx.rotated.saved().last().unwrap().1, "refresh-2");
    assert_eq!(fx.fake.tokens.current_refresh(), "refresh-2");
    // Each refresh used the newest token (the fake rejects retired ones).
    let forms: Vec<_> = fx
        .mock
        .requests()
        .into_iter()
        .filter(|r| r.path == "/oauth/token")
        .map(|r| r.form())
        .collect();
    assert_eq!(forms.len(), 2);
    assert_eq!(forms[0]["refresh_token"], "refresh-0");
    assert_eq!(forms[1]["refresh_token"], "refresh-1");
    assert_eq!(forms[1]["client_secret"], "test-secret");
}

#[test]
fn revoked_refresh_token_fails_connect_with_auth() {
    let (mock, _fake) = FakeBox::start();
    let e = BoxConnector::with_options(support::options(&mock, tuning()))
        .connect(&support::site(Protocol::Box), support::fixed(Some("refresh-old")))
        .err()
        .unwrap();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.message.contains("expired or was revoked"), "{e}");
}

#[test]
fn paginated_listing_and_nested_paths() {
    let fx = setup();
    {
        let mut st = fx.fake.st();
        for i in 0..5 {
            st.add("0", &format!("f{i}.txt"), false, &[i as u8; 10]);
        }
        let a = st.add("0", "A", true, &[]);
        let b = st.add(&a, "B", true, &[]);
        let c = st.add(&b, "C", true, &[]);
        st.add(&c, "deep.bin", false, b"hello box");
    }
    let names: Vec<_> = fx
        .remote
        .list(&RPath::root())
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(
        names,
        ["A", "f0.txt", "f1.txt", "f2.txt", "f3.txt", "f4.txt"]
    );
    assert_eq!(
        fx.mock.count("GET", "/api/folders/0/items"),
        3,
        "page_size 2 → 3 pages"
    );
    let e = fx.remote.stat(&p("/A/B/C/deep.bin")).unwrap();
    assert_eq!((e.size, e.is_dir()), (Some(9), false));
    assert!(e.extra.iter().any(|(k, _)| k == "SHA-1"));
    assert_eq!(get(&*fx.remote, "/A/B/C/deep.bin", 0), b"hello box");
    // Second lookup comes from the cache.
    let before = fx.mock.requests().len();
    fx.remote.stat(&p("/A/B/C/deep.bin")).unwrap();
    assert_eq!(fx.mock.requests().len(), before);
    assert_eq!(
        fx.remote.list(&p("/A/B/C/deep.bin")).unwrap_err().kind,
        ErrorKind::InvalidPath
    );
}

#[test]
fn uploads_below_and_above_threshold_are_byte_equal() {
    let fx = setup();
    let small = bytes(50 * 1024, 1);
    let large = bytes(4 * PART_SIZE as usize + 12_345, 2);
    put(&*fx.remote, "/small.bin", &small, Some(small.len() as u64)).unwrap();
    put(&*fx.remote, "/large.bin", &large, Some(large.len() as u64)).unwrap();
    {
        let st = fx.fake.st();
        assert_eq!(st.at("small.bin").unwrap().data, small);
        assert_eq!(st.at("large.bin").unwrap().data, large);
    }
    assert_eq!(fx.mock.count("POST", "/upload/files/content"), 1);
    assert_eq!(fx.mock.count("POST", "/upload/files/upload_sessions"), 1);
    let parts = fx
        .mock
        .requests()
        .iter()
        .filter(|r| r.method == "PUT" && r.path.starts_with("/upload/files/upload_sessions/"))
        .count();
    assert_eq!(parts, 5);
    assert_eq!(get(&*fx.remote, "/large.bin", 0), large);
    // The cache already knows the new file.
    assert_eq!(
        fx.remote.stat(&p("/large.bin")).unwrap().size,
        Some(large.len() as u64)
    );
    // Unknown size but small: fine.
    put(&*fx.remote, "/nosize.bin", &small, None).unwrap();
    assert_eq!(fx.fake.st().at("nosize.bin").unwrap().data, small);
}

#[test]
fn commit_waits_while_box_is_processing() {
    let fx = setup();
    fx.fake.st().commit_pending = 2;
    let large = bytes(2 * PART_SIZE as usize + 1, 3);
    put(&*fx.remote, "/slow.bin", &large, Some(large.len() as u64)).unwrap();
    assert_eq!(fx.fake.st().at("slow.bin").unwrap().data, large);
}

#[test]
fn overwrite_uploads_new_version_with_if_match() {
    let fx = setup();
    put(&*fx.remote, "/v.txt", b"one", Some(3)).unwrap();
    let id = fx.fake.st().at("v.txt").unwrap().id.clone();
    put(&*fx.remote, "/v.txt", b"two!", Some(4)).unwrap();
    let reqs = fx.mock.requests();
    let ver = reqs
        .iter()
        .find(|r| r.path == format!("/upload/files/{id}/content"))
        .unwrap();
    assert_eq!(ver.header("If-Match"), Some("0"));
    let large = bytes(PART_SIZE as usize * 2 + 10, 4);
    put(&*fx.remote, "/v.txt", &large, Some(large.len() as u64)).unwrap();
    assert_eq!(
        fx.mock
            .count("POST", &format!("/upload/files/{id}/upload_sessions")),
        1
    );
    let st = fx.fake.st();
    let n = st.at("v.txt").unwrap();
    assert_eq!(
        (n.id.as_str(), n.versions, &n.data),
        (id.as_str(), 3, &large)
    );
}

#[test]
fn stale_etag_fails_clearly_then_succeeds_after_refresh() {
    let fx = setup();
    put(&*fx.remote, "/doc.txt", b"mine", Some(4)).unwrap();
    fx.remote.list(&RPath::root()).unwrap();
    // Someone else edits it on Box.
    {
        let mut st = fx.fake.st();
        let id = st.at("doc.txt").unwrap().id.clone();
        st.nodes.get_mut(&id).unwrap().etag += 5;
    }
    let e = fx.remote.remove_file(&p("/doc.txt")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Other);
    assert!(
        e.message.contains("changed on Box since Filewinch looked"),
        "{e}"
    );
    assert!(!e.kind.is_transient());
    assert!(!fx.fake.st().at("doc.txt").unwrap().trashed);
    // The stale entry was dropped, so a deliberate retry sees the new ETag.
    fx.remote.remove_file(&p("/doc.txt")).unwrap();
    assert!(fx.fake.st().at("doc.txt").is_none());
}

#[test]
fn large_upload_of_unknown_size_is_refused() {
    let fx = setup();
    let data = bytes(200 * 1024, 5);
    let e = put(&*fx.remote, "/big.bin", &data, None).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Unsupported);
    assert!(e.message.contains("size"), "{e}");
    assert!(fx.fake.st().at("big.bin").is_none());
}

#[test]
fn dropping_an_unfinished_upload_aborts_the_session() {
    let fx = setup();
    let data = bytes(3 * PART_SIZE as usize, 6);
    let mut w = fx
        .remote
        .write(&p("/partial.bin"), 0, Some(data.len() as u64))
        .unwrap();
    w.write_all(&data[..PART_SIZE as usize + 100]).unwrap();
    drop(w);
    let st = fx.fake.st();
    assert_eq!(st.aborted.len(), 1);
    assert!(st.at("partial.bin").is_none());
}

#[test]
fn short_upload_aborts_and_errors() {
    let fx = setup();
    let data = bytes(3 * PART_SIZE as usize, 7);
    let e = put(
        &*fx.remote,
        "/short.bin",
        &data,
        Some(data.len() as u64 + 1),
    )
    .unwrap_err();
    assert!(e.message.contains("ended after"), "{e}");
    assert_eq!(fx.fake.st().aborted.len(), 1);
}

#[test]
fn ranged_download_follows_redirect_without_token() {
    let fx = setup();
    let data = bytes(10_000, 8);
    put(&*fx.remote, "/r.bin", &data, Some(10_000)).unwrap();
    assert_eq!(get(&*fx.remote, "/r.bin", 1234), &data[1234..]);
    let reqs = fx.mock.requests();
    let dl = reqs
        .iter()
        .rfind(|r| r.path.starts_with("/download/"))
        .unwrap();
    assert_eq!(dl.bearer(), None, "token must not follow the redirect");
    // ureq keeps Range (and drops Authorization) across the redirect.
    assert_eq!(dl.header("Range"), Some("bytes=1234-"));
    assert_eq!(
        fx.remote.read(&p("/nope.bin"), 0).err().unwrap().kind,
        ErrorKind::NotFound
    );
}

#[test]
fn rate_limit_reports_retry_after() {
    let fx = setup();
    fx.fake.st().script.push_back((
        "GET".into(),
        "/api/folders/0/items".into(),
        Reply::json(429, serde_json::json!({"type": "error", "status": 429, "code": "rate_limit_exceeded",
                                           "message": "Request rate limit exceeded, please try again later"}))
            .header("Retry-After", "7"),
    ));
    let e = fx.remote.list(&RPath::root()).unwrap_err();
    assert_eq!(e.kind, ErrorKind::RateLimited);
    assert_eq!(e.retry_after, Some(Duration::from_secs(7)));
    assert!(e.kind.is_transient());
    fx.remote.list(&RPath::root()).unwrap();
}

#[test]
fn not_found_and_permission_errors() {
    let fx = setup();
    let e = fx.remote.stat(&p("/missing/file.txt")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound);
    assert!(e.message.contains("“/missing”"), "{e}");
    let id = fx.fake.st().add("0", "Locked", true, &[]);
    fx.fake.st().forbidden.push(id);
    let e = fx.remote.list(&p("/Locked")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::PermissionDenied);
    assert!(e.detail.unwrap().contains("HTTP 403"));
}

#[test]
fn deletes_go_to_trash_and_folders_must_be_empty() {
    let fx = setup();
    fx.remote.mkdir(&p("/Dir")).unwrap();
    assert_eq!(
        fx.remote.mkdir(&p("/Dir")).unwrap_err().kind,
        ErrorKind::AlreadyExists
    );
    put(&*fx.remote, "/Dir/x.txt", b"x", Some(1)).unwrap();
    let e = fx.remote.remove_dir(&p("/Dir")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidPath);
    assert!(e.message.contains("isn't empty"), "{e}");
    assert_eq!(
        fx.remote.remove_file(&p("/Dir")).unwrap_err().kind,
        ErrorKind::InvalidPath
    );
    let file_id = fx.fake.st().at("Dir/x.txt").unwrap().id.clone();
    fx.remote.remove_file(&p("/Dir/x.txt")).unwrap();
    fx.remote.remove_dir(&p("/Dir")).unwrap();
    let st = fx.fake.st();
    assert!(st.nodes[&file_id].trashed, "file is in the trash, not gone");
    assert!(st.at("Dir").is_none());
    drop(st);
    assert!(fx.remote.list(&RPath::root()).unwrap().is_empty());
}

#[test]
fn copy_and_move() {
    let fx = setup();
    fx.remote.mkdir(&p("/src")).unwrap();
    fx.remote.mkdir(&p("/dst")).unwrap();
    put(&*fx.remote, "/src/a.txt", b"alpha", Some(5)).unwrap();
    fx.remote.copy(&p("/src/a.txt"), &p("/dst/b.txt")).unwrap();
    fx.remote.copy(&p("/src"), &p("/dst/src-copy")).unwrap();
    assert_eq!(get(&*fx.remote, "/dst/b.txt", 0), b"alpha");
    assert_eq!(get(&*fx.remote, "/dst/src-copy/a.txt", 0), b"alpha");
    assert_eq!(
        fx.remote
            .copy(&p("/src/a.txt"), &p("/dst/b.txt"))
            .unwrap_err()
            .kind,
        ErrorKind::AlreadyExists
    );

    fx.remote
        .rename(&p("/src/a.txt"), &p("/dst/moved.txt"))
        .unwrap();
    assert_eq!(
        fx.remote.stat(&p("/src/a.txt")).unwrap_err().kind,
        ErrorKind::NotFound
    );
    assert_eq!(get(&*fx.remote, "/dst/moved.txt", 0), b"alpha");
    let moved = fx
        .mock
        .requests()
        .into_iter()
        .rfind(|r| r.method == "PUT" && r.path.starts_with("/api/files/"))
        .unwrap();
    assert!(moved.header("If-Match").is_some());
    fx.remote.rename(&p("/dst"), &p("/renamed")).unwrap();
    assert_eq!(get(&*fx.remote, "/renamed/moved.txt", 0), b"alpha");
}

#[test]
fn close_rejects_further_calls() {
    let fx = setup();
    let caps = fx.remote.caps();
    assert!(caps.server_copy && caps.server_rename && caps.resume_read && !caps.set_mtime);
    assert_eq!(fx.remote.home().unwrap(), RPath::root());
    fx.remote.close();
    assert_eq!(
        fx.remote.list(&RPath::root()).unwrap_err().kind,
        ErrorKind::Closed
    );
}

fn sha1_of(d: &[u8]) -> Checksum {
    use sha1::Digest;
    Checksum::from_bytes(HashAlgo::Sha1, &sha1::Sha1::digest(d))
}

#[test]
fn sha1_comes_through_on_list_and_stat() {
    let fx = setup();
    {
        let mut st = fx.fake.st();
        let d = st.add("0", "Sync", true, &[]);
        st.add(&d, "a.txt", false, b"box bytes");
        st.add(&d, "sub", true, &[]);
    }
    let list = fx.remote.list(&p("/Sync")).unwrap();
    let by = |n: &str| list.iter().find(|e| e.name == n).unwrap();
    assert_eq!(by("a.txt").checksums, [sha1_of(b"box bytes")]);
    assert!(by("sub").checksums.is_empty(), "folders have no hash");
    let fresh = setup_like(&fx);
    assert_eq!(
        fresh.stat(&p("/Sync/a.txt")).unwrap().checksums,
        [sha1_of(b"box bytes")],
        "stat on a cold cache"
    );
    assert!(fx.remote.stat(&RPath::root()).unwrap().checksums.is_empty());
    let lists: Vec<_> = fx
        .mock
        .requests()
        .into_iter()
        .filter(|q| q.method == "GET" && q.path.ends_with("/items"))
        .collect();
    assert!(!lists.is_empty());
    for q in lists {
        assert!(
            q.query["fields"].split(',').any(|f| f == "sha1"),
            "{:?}",
            q.query
        );
    }
}

/// A second session on the same fake, so nothing comes from the first one's cache.
fn setup_like(fx: &Fx) -> Box<dyn Remote> {
    BoxConnector::with_options(support::options(&fx.mock, tuning()))
        .connect(&support::site(Protocol::Box), fx.rotated.clone())
        .unwrap()
}

#[test]
fn malformed_sha1_and_web_links_give_no_checksum() {
    let fx = setup();
    let good = "C".repeat(40);
    let f = |id: &str, name: &str, sha1: &str| {
        serde_json::json!({"type": "file", "id": id, "name": name, "size": 3, "etag": "0",
                           "sha1": sha1, "modified_at": "2024-03-05T02:20:30-08:00"})
    };
    let entries = serde_json::json!([
        f("1", "good.bin", &good),
        f("2", "short.bin", "abc"),
        f("3", "md5ish.bin", &"0".repeat(32)),
        f("4", "nothex.bin", &"x".repeat(40)),
        f("5", "blank.bin", ""),
        {"type": "folder", "id": "6", "name": "dir", "etag": "0", "sha1": good},
        {"type": "web_link", "id": "7", "name": "Site", "sha1": good,
         "url": "https://example.com/"},
    ]);
    fx.fake.st().script.push_back((
        "GET".into(),
        "/api/folders/0/items".into(),
        Reply::json(
            200,
            serde_json::json!({"entries": entries, "total_count": 7, "next_marker": null}),
        ),
    ));
    let list = fx.remote.list(&RPath::root()).unwrap();
    let names: Vec<_> = list.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        ["good.bin", "short.bin", "md5ish.bin", "nothex.bin", "blank.bin", "dir"],
        "web links have no bytes and aren't listed"
    );
    for e in &list {
        let want: Vec<Checksum> = if e.name == "good.bin" {
            vec![Checksum::new(HashAlgo::Sha1, &"c".repeat(40)).unwrap()]
        } else {
            Vec::new()
        };
        assert_eq!(e.checksums, want, "{}", e.name);
    }
}
