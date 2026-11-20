//! OneDrive backend against a mock Microsoft Graph.

mod mock_onedrive;
mod support;

use std::io::{Read, Write};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use filewinch_cloud::{OneDriveConnector, Provider, Tuning};
use filewinch_core::{Connector, EntryKind, ErrorKind, Protocol, RPath, Remote};
use mock_onedrive::{Graph, MY_DRIVE, OTHER_DRIVE};

fn tuning() -> Tuning {
    Tuning {
        simple_max: 400 * 1024,
        chunk: 320 * 1024,
        page_size: 2,
        ..Tuning::for_provider(Provider::OneDrive)
    }
}

fn connect(g: &Graph) -> Box<dyn Remote> {
    let c = OneDriveConnector::with_options(support::options(&g.mock, tuning()));
    assert_eq!(c.protocols(), &[Protocol::OneDrive]);
    c.connect(&support::site(Protocol::OneDrive), support::secrets())
        .unwrap()
}

fn p(s: &str) -> RPath {
    RPath::new(s)
}

/// My files: Docs/{a.txt, b.txt, c.txt, Deep/{Deeper/{leaf.bin}}}, empty/, top.txt
fn seed(g: &Graph) {
    let mut st = g.state.lock().unwrap();
    let docs = st.add(MY_DRIVE, Some("root"), "Docs", true, &[]);
    for n in ["a.txt", "b.txt", "c.txt"] {
        st.add(MY_DRIVE, Some(&docs), n, false, n.as_bytes());
    }
    let deep = st.add(MY_DRIVE, Some(&docs), "Deep", true, &[]);
    let deeper = st.add(MY_DRIVE, Some(&deep), "Deeper", true, &[]);
    st.add(
        MY_DRIVE,
        Some(&deeper),
        "leaf.bin",
        false,
        &support::bytes(5000, 9),
    );
    st.add(MY_DRIVE, Some("root"), "empty", true, &[]);
    st.add(MY_DRIVE, Some("root"), "top.txt", false, b"top");
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
    let mut sink = r.write(&p(path), 0, size)?;
    for c in data.chunks(100_000) {
        sink.write_all(c)?;
    }
    sink.finish()
}

#[test]
fn virtual_root_and_home() {
    let g = Graph::start(false);
    let r = connect(&g);
    assert_eq!(r.home().unwrap(), p("/My files"));
    let names: Vec<_> = r
        .list(&RPath::root())
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["My files", "Shared with me"]);
    assert!(r.stat(&p("/Shared with me")).unwrap().is_dir());
    assert_eq!(r.list(&p("/Nope")).unwrap_err().kind, ErrorKind::NotFound);
    let caps = r.caps();
    assert!(
        caps.resume_read
            && caps.server_copy
            && caps.server_rename
            && caps.set_mtime
            && !caps.resume_write
    );
}

#[test]
fn pagination_and_nested_resolution() {
    let g = Graph::start(false);
    seed(&g);
    let r = connect(&g);
    let docs = r.list(&p("/My files/Docs")).unwrap();
    let names: Vec<_> = docs.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["Deep", "a.txt", "b.txt", "c.txt"]);
    assert!(
        g.mock
            .requests()
            .iter()
            .any(|q| q.query.contains_key("$skiptoken")),
        "followed nextLink"
    );
    let a = docs.iter().find(|e| e.name == "a.txt").unwrap();
    assert_eq!(
        (a.kind, a.size, a.path.as_str()),
        (EntryKind::File, Some(5), "/My files/Docs/a.txt")
    );
    assert!(a.modified.is_some());

    let leaf = r.stat(&p("/My files/Docs/Deep/Deeper/leaf.bin")).unwrap();
    assert_eq!(leaf.size, Some(5000));
    assert_eq!(
        read_all(&*r, "/My files/Docs/Deep/Deeper/leaf.bin", 0),
        support::bytes(5000, 9)
    );
    // Case-insensitive like OneDrive itself.
    assert_eq!(r.stat(&p("/My files/docs/A.TXT")).unwrap().name, "a.txt");
    let e = r.stat(&p("/My files/Docs/missing.txt")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound);
    assert!(e.message.contains("missing.txt"), "{e}");
    assert_eq!(
        r.list(&p("/My files/top.txt")).unwrap_err().kind,
        ErrorKind::InvalidPath
    );
}

#[test]
fn refreshes_token_on_401() {
    let g = Graph::start(false);
    seed(&g);
    let r = connect(&g);
    r.list(&p("/My files")).unwrap();
    let before = g.tokens.refreshes.load(Ordering::SeqCst);
    g.tokens.expire_all();
    assert_eq!(r.list(&p("/My files/Docs")).unwrap().len(), 4);
    assert_eq!(g.tokens.refreshes.load(Ordering::SeqCst), before + 1);
}

#[test]
fn rotated_refresh_token_is_saved_and_reused() {
    let g = Graph::start(true);
    seed(&g);
    let keychain = Arc::new(support::Recording::default());
    let r = OneDriveConnector::with_options(support::options(&g.mock, tuning()))
        .connect(&support::site(Protocol::OneDrive), keychain.clone())
        .unwrap();
    assert_eq!(
        keychain.saved(),
        [("oauth.site-1".to_string(), "refresh-1".to_string())]
    );
    // The mock retired refresh-0; the next refresh only works with the rotated token.
    g.tokens.expire_all();
    r.list(&p("/My files")).unwrap();
    assert_eq!(keychain.saved().last().unwrap().1, "refresh-2");
    assert_eq!(g.tokens.current_refresh(), "refresh-2");
}

#[test]
fn revoked_sign_in_fails_at_connect() {
    let g = Graph::start(false);
    let c = OneDriveConnector::with_options(support::options(&g.mock, tuning()));
    let e = c
        .connect(&support::site(Protocol::OneDrive), support::fixed(Some("refresh-revoked")))
        .err()
        .unwrap();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.message.contains("expired or was revoked"), "{e}");
}

#[test]
fn small_upload_is_one_put_and_byte_equal() {
    let g = Graph::start(false);
    seed(&g);
    let r = connect(&g);
    let data = support::bytes(300 * 1024, 1);
    upload(
        &*r,
        "/My files/Docs/small.bin",
        &data,
        Some(data.len() as u64),
    )
    .unwrap();
    assert_eq!(
        g.state.lock().unwrap().at("Docs/small.bin").unwrap().data,
        data
    );
    assert!(
        g.state.lock().unwrap().sessions.is_empty()
            && g.state.lock().unwrap().deleted_sessions.is_empty()
    );
    // Unknown size but small still works, and replaces.
    upload(&*r, "/My files/Docs/small.bin", b"replaced", None).unwrap();
    assert_eq!(read_all(&*r, "/My files/Docs/small.bin", 0), b"replaced");
}

#[test]
fn large_upload_uses_session_and_is_byte_equal() {
    let g = Graph::start(false);
    seed(&g);
    let r = connect(&g);
    let data = support::bytes(1_000_000, 2); // 3 fragments of 320 KiB + tail
    g.short_ack.store(true, Ordering::SeqCst); // first fragment half-acknowledged: must resend
    upload(
        &*r,
        "/My files/Docs/Deep/big.bin",
        &data,
        Some(data.len() as u64),
    )
    .unwrap();
    assert_eq!(
        g.state
            .lock()
            .unwrap()
            .at("Docs/Deep/big.bin")
            .unwrap()
            .data,
        data
    );
    let puts = g
        .mock
        .requests()
        .into_iter()
        .filter(|q| q.path.starts_with("/upsession/"))
        .count();
    assert!(puts >= 5, "fragments sent: {puts}");
    assert_eq!(
        r.stat(&p("/My files/Docs/Deep/big.bin")).unwrap().size,
        Some(1_000_000)
    );
}

#[test]
fn large_upload_of_unknown_size_is_refused_clearly() {
    let g = Graph::start(false);
    seed(&g);
    let r = connect(&g);
    let e = upload(
        &*r,
        "/My files/huge.bin",
        &support::bytes(500 * 1024, 3),
        None,
    )
    .unwrap_err();
    let msg = e.to_string();
    assert!(msg.contains("needs to know a file's size"), "{msg}");
    assert!(g.state.lock().unwrap().at("huge.bin").is_none());
}

#[test]
fn dropping_unfinished_session_deletes_it() {
    let g = Graph::start(false);
    seed(&g);
    let r = connect(&g);
    let mut sink = r
        .write(&p("/My files/partial.bin"), 0, Some(2_000_000))
        .unwrap();
    sink.write_all(&support::bytes(700 * 1024, 4)).unwrap();
    drop(sink);
    let st = g.state.lock().unwrap();
    assert_eq!(st.deleted_sessions, ["s1"]);
    assert!(st.at("partial.bin").is_none());
}

#[test]
fn ranged_download_follows_redirect_without_token() {
    let g = Graph::start(false);
    seed(&g);
    let r = connect(&g);
    let full = support::bytes(5000, 9);
    assert_eq!(
        read_all(&*r, "/My files/Docs/Deep/Deeper/leaf.bin", 1234),
        full[1234..]
    );
    let dl: Vec<_> = g
        .mock
        .requests()
        .into_iter()
        .filter(|q| q.path.starts_with("/dl/"))
        .collect();
    assert!(!dl.is_empty());
    assert!(
        dl.iter().all(|q| q.header("Authorization").is_none()),
        "bearer leaked to the download host"
    );
    assert_eq!(dl.last().unwrap().header("Range"), Some("bytes=1234-"));
    // A server that ignores Range still yields the right bytes.
    g.ignore_range.store(true, Ordering::SeqCst);
    assert_eq!(
        read_all(&*r, "/My files/Docs/Deep/Deeper/leaf.bin", 4000),
        full[4000..]
    );
}

#[test]
fn rate_limit_carries_retry_after() {
    let g = Graph::start(false);
    seed(&g);
    let r = connect(&g);
    g.throttle.store(1, Ordering::SeqCst);
    let e = r.list(&p("/My files/Docs")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::RateLimited);
    assert_eq!(e.retry_after, Some(Duration::from_secs(7)));
    assert!(e.kind.is_transient());
    assert_eq!(r.list(&p("/My files/Docs")).unwrap().len(), 4);
}

#[test]
fn shared_with_me_and_permission_errors() {
    let g = Graph::start(false);
    {
        let mut st = g.state.lock().unwrap();
        let plans = st.add(OTHER_DRIVE, Some("other-root"), "Plans", true, &[]);
        st.add(OTHER_DRIVE, Some(&plans), "q1.txt", false, b"quarter one");
        let memo = st.add(OTHER_DRIVE, Some("other-root"), "memo.txt", false, b"memo");
        st.shared = vec![plans.clone(), memo];
        st.readonly = vec![plans];
    }
    let r = connect(&g);
    let shared = r.list(&p("/Shared with me")).unwrap();
    let names: Vec<_> = shared.iter().map(|e| (e.name.as_str(), e.kind)).collect();
    assert_eq!(
        names,
        [("Plans", EntryKind::Dir), ("memo.txt", EntryKind::File)]
    );
    let inner = r.list(&p("/Shared with me/Plans")).unwrap();
    assert_eq!(inner[0].name, "q1.txt");
    assert!(g.mock.requests().iter().any(|q| {
        q.path
            .starts_with(&format!("/api/drives/{OTHER_DRIVE}/items/"))
    }));
    assert_eq!(
        read_all(&*r, "/Shared with me/Plans/q1.txt", 0),
        b"quarter one"
    );

    // The sharer only gave read access.
    let e = upload(&*r, "/Shared with me/Plans/new.txt", b"x", Some(1)).unwrap_err();
    assert_eq!(e.kind, ErrorKind::PermissionDenied);
    assert_eq!(
        r.remove_file(&p("/Shared with me/Plans/q1.txt"))
            .unwrap_err()
            .kind,
        ErrorKind::PermissionDenied
    );
    // The virtual folders themselves can't be changed.
    assert_eq!(
        r.mkdir(&p("/Shared with me/New")).unwrap_err().kind,
        ErrorKind::PermissionDenied
    );
    assert_eq!(
        r.remove_file(&p("/Shared with me/memo.txt"))
            .unwrap_err()
            .kind,
        ErrorKind::PermissionDenied
    );
    assert_eq!(
        r.remove_dir(&p("/My files")).unwrap_err().kind,
        ErrorKind::PermissionDenied
    );
    assert_eq!(
        r.mkdir(&p("/Elsewhere")).unwrap_err().kind,
        ErrorKind::NotFound
    );
}

#[test]
fn mkdir_rename_delete_and_remove_dir() {
    let g = Graph::start(false);
    seed(&g);
    let r = connect(&g);
    r.mkdir(&p("/My files/New")).unwrap();
    assert_eq!(
        r.mkdir(&p("/My files/New")).unwrap_err().kind,
        ErrorKind::AlreadyExists
    );
    r.rename(&p("/My files/top.txt"), &p("/My files/New/moved.txt"))
        .unwrap();
    assert_eq!(read_all(&*r, "/My files/New/moved.txt", 0), b"top");
    assert_eq!(
        r.stat(&p("/My files/top.txt")).unwrap_err().kind,
        ErrorKind::NotFound
    );
    assert_eq!(
        r.rename(&p("/My files/Docs/a.txt"), &p("/My files/Docs/b.txt"))
            .unwrap_err()
            .kind,
        ErrorKind::AlreadyExists
    );

    let e = r.remove_dir(&p("/My files/Docs")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidPath);
    assert!(e.message.contains("isn't empty"), "{e}");
    assert_eq!(
        r.remove_dir(&p("/My files/top.txt")).unwrap_err().kind,
        ErrorKind::NotFound
    );
    assert_eq!(
        r.remove_file(&p("/My files/Docs")).unwrap_err().kind,
        ErrorKind::InvalidPath
    );
    r.remove_dir(&p("/My files/empty")).unwrap();
    r.remove_file(&p("/My files/New/moved.txt")).unwrap();
    assert!(r.list(&p("/My files/New")).unwrap().is_empty());
    assert!(g.state.lock().unwrap().at("empty").is_none());
}

#[test]
fn async_copy_completes_and_fails() {
    let g = Graph::start(false);
    seed(&g);
    let r = connect(&g);
    r.copy(&p("/My files/Docs/a.txt"), &p("/My files/copy.txt"))
        .unwrap();
    assert_eq!(read_all(&*r, "/My files/copy.txt", 0), b"a.txt");
    let polls: Vec<_> = g
        .mock
        .requests()
        .into_iter()
        .filter(|q| q.path.starts_with("/monitor/"))
        .collect();
    assert!(polls.len() >= 2 && polls.iter().all(|q| q.header("Authorization").is_none()));

    r.copy(&p("/My files/Docs/Deep"), &p("/My files/Deep copy"))
        .unwrap();
    assert_eq!(
        r.stat(&p("/My files/Deep copy/Deeper/leaf.bin"))
            .unwrap()
            .size,
        Some(5000)
    );

    g.copy_fails.store(true, Ordering::SeqCst);
    let e = r
        .copy(&p("/My files/Docs/b.txt"), &p("/My files/b2.txt"))
        .unwrap_err();
    assert!(e.message.contains("destination is locked"), "{e}");
}

#[test]
fn set_mtime_patches_file_system_info() {
    let g = Graph::start(false);
    seed(&g);
    let r = connect(&g);
    let t = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    r.set_mtime(&p("/My files/top.txt"), t).unwrap();
    assert_eq!(
        g.state.lock().unwrap().at("top.txt").unwrap().mtime,
        "2020-09-13T12:26:40Z"
    );
    assert_eq!(r.stat(&p("/My files/top.txt")).unwrap().modified, Some(t));
}

#[test]
fn close_makes_calls_fail() {
    let g = Graph::start(false);
    seed(&g);
    let r = connect(&g);
    r.close();
    assert_eq!(r.list(&p("/My files")).unwrap_err().kind, ErrorKind::Closed);
    assert_eq!(r.list(&RPath::root()).unwrap_err().kind, ErrorKind::Closed);
}
