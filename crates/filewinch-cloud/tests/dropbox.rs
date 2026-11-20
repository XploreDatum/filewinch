//! Dropbox backend against an in-memory fake of the v2 API.

mod mock_dropbox;
mod support;

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

use filewinch_cloud::{ConnectorOptions, DropboxConnector, Provider, Tuning};
use filewinch_core::{Checksum, Connector, EntryKind, ErrorKind, HashAlgo, Protocol, RPath, Remote};
use mock_dropbox::FakeDropbox;
use serde_json::json;
use support::{bytes, options, site};

fn tuning() -> Tuning {
    Tuning {
        simple_max: 64 * 1024,
        chunk: 16 * 1024,
        page_size: 2,
        ..Tuning::for_provider(Provider::Dropbox)
    }
}

fn connect_with(_fake: &FakeDropbox, opts: ConnectorOptions) -> Box<dyn Remote> {
    DropboxConnector::with_options(opts)
        .connect(&site(Protocol::Dropbox), support::secrets())
        .unwrap()
}

fn connect(fake: &FakeDropbox) -> Box<dyn Remote> {
    connect_with(fake, options(&fake.mock, tuning()))
}

fn p(s: &str) -> RPath {
    RPath::new(s)
}

fn read_all(r: &dyn Remote, path: &str, offset: u64) -> Vec<u8> {
    let mut out = Vec::new();
    r.read(&p(path), offset)
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    out
}

fn upload(
    r: &dyn Remote,
    path: &str,
    data: &[u8],
    size: Option<u64>,
) -> filewinch_core::Result<()> {
    let mut w = r.write(&p(path), 0, size)?;
    for c in data.chunks(5000) {
        w.write_all(c)?;
    }
    w.finish()
}

#[test]
fn lists_with_pagination_and_nested_paths() {
    let fake = FakeDropbox::start(false);
    {
        let mut st = fake.state.lock().unwrap();
        for i in 0..5 {
            st.put_file(&format!("/Work/Reports/r{i}.txt"), b"x");
        }
        st.put_file("/Work/notes.md", b"hello");
    }
    let r = connect(&fake);
    assert_eq!(r.home().unwrap(), RPath::root());
    let names: Vec<_> = r
        .list(&p("/Work/Reports"))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["r0.txt", "r1.txt", "r2.txt", "r3.txt", "r4.txt"]);
    assert!(
        fake.mock.count("POST", "/api/files/list_folder/continue") >= 2,
        "page_size 2 must paginate"
    );

    let top = r.list(&p("/Work")).unwrap();
    let dir = top.iter().find(|e| e.name == "Reports").unwrap();
    assert_eq!(
        (dir.kind, dir.path.as_str(), dir.size),
        (EntryKind::Dir, "/Work/Reports", None)
    );
    let f = r.stat(&p("/Work/notes.md")).unwrap();
    assert_eq!((f.kind, f.size), (EntryKind::File, Some(5)));
    assert!(f.modified.is_some());
    assert!(f.extra.iter().any(|(k, _)| k == "Revision"));
    assert!(r.stat(&RPath::root()).unwrap().is_dir());
}

#[test]
fn upload_small_and_large_are_byte_equal() {
    let fake = FakeDropbox::start(false);
    let r = connect(&fake);
    let small = bytes(10_000, 1);
    upload(
        r.as_ref(),
        "/in/small.bin",
        &small,
        Some(small.len() as u64),
    )
    .unwrap();
    assert_eq!(fake.mock.count("POST", "/upload/files/upload"), 1);
    assert_eq!(
        fake.state.lock().unwrap().file("/in/small.bin").unwrap(),
        small
    );

    // Above simple_max with the size known up front, and again with it unknown.
    for (name, size_known) in [("/in/big.bin", true), ("/in/big2.bin", false)] {
        let big = bytes(150_001, 2);
        upload(
            r.as_ref(),
            name,
            &big,
            size_known.then_some(big.len() as u64),
        )
        .unwrap();
        assert_eq!(
            fake.state.lock().unwrap().file(name).unwrap(),
            big,
            "{name}"
        );
        assert_eq!(read_all(r.as_ref(), name, 0), big);
    }
    assert_eq!(
        fake.mock
            .count("POST", "/upload/files/upload_session/start"),
        2
    );
    assert!(
        fake.mock
            .count("POST", "/upload/files/upload_session/append_v2")
            >= 16
    );
    assert_eq!(
        fake.mock
            .count("POST", "/upload/files/upload_session/finish"),
        2
    );

    // Overwrite keeps one file.
    upload(r.as_ref(), "/in/small.bin", b"new", None).unwrap();
    assert_eq!(
        fake.state.lock().unwrap().file("/in/small.bin").unwrap(),
        b"new"
    );
}

#[test]
fn dropped_upload_commits_nothing() {
    let fake = FakeDropbox::start(false);
    let r = connect(&fake);
    let mut w = r.write(&p("/half.bin"), 0, None).unwrap();
    w.write_all(&bytes(100_000, 3)).unwrap();
    drop(w);
    assert!(fake.state.lock().unwrap().file("/half.bin").is_none());
    assert_eq!(
        fake.mock
            .count("POST", "/upload/files/upload_session/finish"),
        0
    );

    // A short write (size promised but not delivered) fails and commits nothing.
    let mut w = r.write(&p("/short.bin"), 0, Some(200_000)).unwrap();
    w.write_all(&bytes(1000, 4)).unwrap();
    assert!(w.finish().is_err());
    assert!(fake.state.lock().unwrap().file("/short.bin").is_none());
}

#[test]
fn ranged_download_resumes() {
    let fake = FakeDropbox::start(false);
    let data = bytes(5000, 5);
    fake.state.lock().unwrap().put_file("/f.bin", &data);
    let r = connect(&fake);
    assert_eq!(read_all(r.as_ref(), "/f.bin", 1234), &data[1234..]);
    let req = fake
        .mock
        .requests()
        .into_iter()
        .rfind(|q| q.path == "/upload/files/download")
        .unwrap();
    assert_eq!(req.header("Range"), Some("bytes=1234-"));
    assert!(r.caps().resume_read && !r.caps().resume_write);
}

#[test]
fn non_ascii_paths_travel_in_the_header() {
    let fake = FakeDropbox::start(false);
    let r = connect(&fake);
    upload(r.as_ref(), "/Café/naïve 😀.txt", b"bonjour", None).unwrap();
    assert_eq!(read_all(r.as_ref(), "/Café/naïve 😀.txt", 0), b"bonjour");
    let req = fake
        .mock
        .requests()
        .into_iter()
        .find(|q| q.path == "/upload/files/upload")
        .unwrap();
    assert!(req.header("Dropbox-API-Arg").unwrap().is_ascii());
}

#[test]
fn mkdir_rename_copy_delete() {
    let fake = FakeDropbox::start(false);
    let r = connect(&fake);
    r.mkdir(&p("/A")).unwrap();
    assert_eq!(
        r.mkdir(&p("/A")).unwrap_err().kind,
        ErrorKind::AlreadyExists
    );
    upload(r.as_ref(), "/A/one.txt", b"1", None).unwrap();
    r.copy(&p("/A/one.txt"), &p("/A/two.txt")).unwrap();
    r.rename(&p("/A/one.txt"), &p("/A/uno.txt")).unwrap();
    assert_eq!(read_all(r.as_ref(), "/A/two.txt", 0), b"1");
    assert_eq!(
        r.stat(&p("/A/one.txt")).unwrap_err().kind,
        ErrorKind::NotFound
    );

    let e = r.remove_dir(&p("/A")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidPath, "{e}");
    assert_eq!(
        r.remove_file(&p("/A")).unwrap_err().kind,
        ErrorKind::InvalidPath
    );
    r.remove_file(&p("/A/two.txt")).unwrap();
    r.remove_file(&p("/A/uno.txt")).unwrap();
    r.remove_dir(&p("/A")).unwrap();
    assert!(fake.state.lock().unwrap().nodes.is_empty());
    let caps = r.caps();
    assert!(caps.server_copy && caps.server_rename && caps.real_dirs);
}

#[test]
fn not_found_and_permission_errors() {
    let fake = FakeDropbox::start(false);
    {
        let mut st = fake.state.lock().unwrap();
        st.put_file("/Shared/locked.txt", b"x");
        st.read_only.push("/Shared".into());
    }
    let r = connect(&fake);
    let e = r.stat(&p("/nope.txt")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound);
    assert_eq!(e.message, "“/nope.txt” wasn't found on Dropbox.");
    assert_eq!(r.list(&p("/nope")).unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(
        r.list(&p("/Shared/locked.txt")).unwrap_err().kind,
        ErrorKind::InvalidPath
    );
    let e = upload(r.as_ref(), "/Shared/new.txt", b"x", None).unwrap_err();
    assert_eq!(e.kind, ErrorKind::PermissionDenied, "{e}");
    assert_eq!(
        r.remove_file(&p("/Shared/locked.txt")).unwrap_err().kind,
        ErrorKind::PermissionDenied
    );
}

#[test]
fn rate_limit_carries_retry_after() {
    let fake = FakeDropbox::start(false);
    let r = connect(&fake);
    fake.state.lock().unwrap().fail_next = Some((
        429,
        json!({"error_summary": "too_many_requests/..", "error": {"reason": {".tag": "too_many_requests"}, "retry_after": 7}}),
        Some("7".into()),
    ));
    let e = r.list(&RPath::root()).unwrap_err();
    assert_eq!(e.kind, ErrorKind::RateLimited);
    assert_eq!(e.retry_after, Some(Duration::from_secs(7)));
    assert!(e.kind.is_transient());

    // Body-only retry_after (no header) is honoured too.
    fake.state.lock().unwrap().fail_next = Some((
        429,
        json!({"error_summary": "too_many_write_operations/", "error": {"retry_after": 3}}),
        None,
    ));
    assert_eq!(
        r.mkdir(&p("/x")).unwrap_err().retry_after,
        Some(Duration::from_secs(3))
    );

    fake.state.lock().unwrap().fail_next = Some((503, json!({}), None));
    assert_eq!(r.list(&RPath::root()).unwrap_err().kind, ErrorKind::Network);
}

#[test]
fn refreshes_once_on_401() {
    let fake = FakeDropbox::start(false);
    let r = connect(&fake);
    r.list(&RPath::root()).unwrap();
    assert_eq!(
        fake.tokens
            .refreshes
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    fake.tokens.expire_all();
    r.list(&RPath::root()).unwrap();
    assert_eq!(
        fake.tokens
            .refreshes
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    let unauth = fake
        .mock
        .requests()
        .iter()
        .filter(|q| q.bearer() == Some("access-1"))
        .count();
    assert!(
        unauth >= 2,
        "the expired token was used, rejected, then replaced"
    );
}

#[test]
fn revoked_sign_in_fails_at_connect() {
    let fake = FakeDropbox::start(false);
    let e = DropboxConnector::with_options(options(&fake.mock, tuning()))
        .connect(&site(Protocol::Dropbox), support::fixed(Some("revoked")))
        .err()
        .unwrap();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.message.contains("expired or was revoked"), "{e}");

    let e = DropboxConnector::with_options(options(&fake.mock, tuning()))
        .connect(&site(Protocol::Dropbox), support::fixed(None))
        .err()
        .unwrap();
    assert_eq!(e.kind, ErrorKind::Auth);
}

#[test]
fn rotated_refresh_token_is_saved_through_secrets() {
    let fake = FakeDropbox::start(true);
    let keychain = Arc::new(support::Recording::default());
    let r = DropboxConnector::with_options(options(&fake.mock, tuning()))
        .connect(&site(Protocol::Dropbox), keychain.clone())
        .unwrap();
    fake.tokens.expire_all();
    r.list(&RPath::root()).unwrap();
    assert_eq!(
        keychain.saved(),
        [
            ("oauth.site-1".to_string(), "refresh-1".to_string()),
            ("oauth.site-1".to_string(), "refresh-2".to_string())
        ]
    );
    assert_eq!(fake.tokens.current_refresh(), "refresh-2");
}

#[test]
fn close_makes_calls_fail() {
    let fake = FakeDropbox::start(false);
    let r = connect(&fake);
    r.close();
    assert_eq!(r.list(&RPath::root()).unwrap_err().kind, ErrorKind::Closed);
}

#[test]
fn content_hash_comes_through_on_list_and_stat() {
    let fake = FakeDropbox::start(false);
    let data = bytes(5 * 1024 * 1024, 3); // two 4 MiB blocks
    {
        let mut st = fake.state.lock().unwrap();
        st.put_file("/Sync/big.bin", &data);
        st.put_file("/Sync/empty.txt", b"");
        st.ensure_dir("/Sync/sub");
    }
    let r = connect(&fake);
    let want = |d: &[u8]| {
        vec![Checksum::new(HashAlgo::DropboxContent, &mock_dropbox::content_hash(d)).unwrap()]
    };
    let list = r.list(&p("/Sync")).unwrap();
    let by = |n: &str| list.iter().find(|e| e.name == n).unwrap();
    assert_eq!(by("big.bin").checksums, want(&data));
    assert_eq!(
        by("empty.txt").checksums[0].hex,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "an empty file's content_hash is SHA-256 of nothing"
    );
    assert!(by("sub").checksums.is_empty(), "folders have no hash");
    assert_eq!(r.stat(&p("/Sync/big.bin")).unwrap().checksums, want(&data));
    assert!(r.stat(&p("/Sync/sub")).unwrap().checksums.is_empty());
    assert!(r.stat(&RPath::root()).unwrap().checksums.is_empty());
}

#[test]
fn exports_and_malformed_content_hashes_are_dropped() {
    let fake = FakeDropbox::start(false);
    fake.state.lock().unwrap().ensure_dir("/Docs");
    let r = connect(&fake);
    let h = "AB".repeat(32);
    let file = |name: &str, extra: serde_json::Value| {
        let mut v = json!({".tag": "file", "name": name, "path_display": format!("/Docs/{name}"),
                           "id": format!("id:{name}"), "rev": "0000001", "size": 3,
                           "server_modified": "2024-03-05T10:20:31Z"});
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    };
    let entries = json!([
        file("good.bin", json!({"content_hash": h, "is_downloadable": true})),
        file("short.bin", json!({"content_hash": "abc"})),
        file("nothex.bin", json!({"content_hash": "g".repeat(64)})),
        file("missing.bin", json!({})),
        file("Notes.paper", json!({"content_hash": h, "is_downloadable": false})),
        file("Sheet.gsheet", json!({"content_hash": h,
            "export_info": {"export_as": "xlsx", "export_options": ["xlsx"]}})),
        {".tag": "folder", "name": "dir", "path_display": "/Docs/dir", "id": "id:dir",
         "content_hash": h},
    ]);
    fake.state.lock().unwrap().fail_next = Some((
        200,
        json!({"entries": entries, "cursor": "c", "has_more": false}),
        None,
    ));
    let list = r.list(&p("/Docs")).unwrap();
    assert_eq!(list.len(), 7);
    for e in &list {
        let want: Vec<Checksum> = if e.name == "good.bin" {
            vec![Checksum::new(HashAlgo::DropboxContent, &"ab".repeat(32)).unwrap()]
        } else {
            Vec::new()
        };
        assert_eq!(e.checksums, want, "{}", e.name);
    }
}
