//! Integration tests against a real WebDAV server: `rclone serve webdav` with Basic auth
//! (and TLS with a self-signed certificate for the trust test).

mod common;

use std::io::{Read, Write};
use std::time::Duration;

use common::*;
use filewinch_core::{Auth, ErrorKind, Protocol, RPath, Remote};
use filewinch_testkit::{FaultProxy, Kind, Server};

fn upload(remote: &dyn Remote, path: &str, data: &[u8]) {
    let mut sink = remote.write(&RPath::new(path), 0, Some(data.len() as u64)).unwrap();
    sink.write_all(data).unwrap();
    sink.finish().unwrap();
}

fn download(remote: &dyn Remote, path: &str, offset: u64) -> Vec<u8> {
    let mut out = Vec::new();
    remote.read(&RPath::new(path), offset).unwrap().read_to_end(&mut out).unwrap();
    out
}

#[test]
fn connects_and_rejects_wrong_password_and_missing_login() {
    let srv = Server::start(Kind::WebDav, base());
    let remote = connect_rclone(&srv, srv.port);
    assert_eq!(remote.home().unwrap(), RPath::root());
    let caps = remote.caps();
    assert!(caps.resume_read && !caps.resume_write && caps.real_dirs && caps.server_rename && caps.server_copy);

    let s = site(Protocol::WebDav, "127.0.0.1", srv.port, &srv.user);
    let err = connect_with(&s, "wrong").err().expect("wrong password must fail");
    assert_eq!(err.kind, ErrorKind::Auth, "{err}");
    assert!(err.message.contains(&format!("{}@127.0.0.1", srv.user)), "{err}");

    let mut anon = s.clone();
    anon.user.clear();
    anon.auth = Auth::None;
    let err = connect_with(&anon, "").err().expect("no login must fail");
    assert_eq!(err.kind, ErrorKind::Auth, "{err}");

    let err = filewinch_core::Connector::connect(&filewinch_webdav::WebDavConnector, &s, std::sync::Arc::new(filewinch_core::MemorySecrets::new()))
        .err()
        .unwrap();
    assert_eq!(err.kind, ErrorKind::Auth);
    assert!(err.message.contains("no saved password"), "{err}");

    let mut nowhere = s.clone();
    let port = filewinch_testkit::refused_port();
    nowhere.port = Some(port);
    assert_eq!(connect_with(&nowhere, "x").err().unwrap().kind, ErrorKind::Network);
}

#[test]
fn crud() {
    let srv = Server::start(Kind::WebDav, base());
    let r = connect_rclone(&srv, srv.port);

    r.mkdir(&RPath::new("/docs")).unwrap();
    // rclone answers 201 to MKCOL on an existing folder (servers that send 405 are covered
    // by the mock tests), so a repeat is either fine or AlreadyExists.
    if let Err(e) = r.mkdir(&RPath::new("/docs")) {
        assert_eq!(e.kind, ErrorKind::AlreadyExists, "{e}");
    }
    assert_eq!(r.mkdir(&RPath::new("/nope/sub")).unwrap_err().kind, ErrorKind::NotFound);

    upload(&*r, "/docs/a.txt", b"hello");
    // Unknown size: chunked upload.
    let mut sink = r.write(&RPath::new("/docs/b.txt"), 0, None).unwrap();
    sink.write_all(b"chunked body").unwrap();
    sink.finish().unwrap();
    assert_eq!(std::fs::read(srv.root.join("docs/b.txt")).unwrap(), b"chunked body");

    let mut list = r.list(&RPath::new("/docs")).unwrap();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    let names: Vec<_> = list.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["a.txt", "b.txt"]);
    assert_eq!(list[0].size, Some(5));
    assert_eq!(list[0].path, RPath::new("/docs/a.txt"));
    assert!(list[0].modified.is_some());

    let root = r.list(&RPath::root()).unwrap();
    assert_eq!(root.len(), 1);
    assert!(root[0].is_dir() && root[0].name == "docs");

    let st = r.stat(&RPath::new("/docs/a.txt")).unwrap();
    assert_eq!((st.name.as_str(), st.size, st.is_dir()), ("a.txt", Some(5), false));
    assert!(r.stat(&RPath::new("/docs")).unwrap().is_dir());
    assert!(r.stat(&RPath::root()).unwrap().is_dir());
    assert_eq!(r.stat(&RPath::new("/docs/missing")).unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(r.list(&RPath::new("/missing")).unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(r.read(&RPath::new("/missing"), 0).err().unwrap().kind, ErrorKind::NotFound);

    // Rename: onto an existing name is refused, otherwise it moves.
    let err = r.rename(&RPath::new("/docs/a.txt"), &RPath::new("/docs/b.txt")).unwrap_err();
    assert_eq!(err.kind, ErrorKind::AlreadyExists, "{err}");
    assert_eq!(download(&*r, "/docs/b.txt", 0), b"chunked body");
    r.rename(&RPath::new("/docs/a.txt"), &RPath::new("/docs/c.txt")).unwrap();
    assert_eq!(r.stat(&RPath::new("/docs/a.txt")).unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(r.rename(&RPath::new("/docs/zz"), &RPath::new("/docs/yy")).unwrap_err().kind, ErrorKind::NotFound);

    // Copy, including a folder.
    r.copy(&RPath::new("/docs/c.txt"), &RPath::new("/docs/d.txt")).unwrap();
    assert_eq!(download(&*r, "/docs/d.txt", 0), b"hello");
    assert_eq!(
        r.copy(&RPath::new("/docs/c.txt"), &RPath::new("/docs/d.txt")).unwrap_err().kind,
        ErrorKind::AlreadyExists
    );
    r.copy(&RPath::new("/docs"), &RPath::new("/docs2")).unwrap();
    assert_eq!(r.list(&RPath::new("/docs2")).unwrap().len(), 3);
    r.rename(&RPath::new("/docs2"), &RPath::new("/docs3")).unwrap();
    assert!(r.stat(&RPath::new("/docs3")).unwrap().is_dir());

    // Deleting: never a folder through remove_file, never a non-empty folder.
    assert_eq!(r.remove_file(&RPath::new("/docs")).unwrap_err().kind, ErrorKind::InvalidPath);
    let err = r.remove_dir(&RPath::new("/docs")).unwrap_err();
    assert_eq!(err.kind, ErrorKind::InvalidPath, "{err}");
    assert!(err.message.contains("isn't empty"), "{err}");
    assert!(srv.root.join("docs/c.txt").exists());
    assert_eq!(r.remove_dir(&RPath::new("/docs/c.txt")).unwrap_err().kind, ErrorKind::InvalidPath);
    for f in ["b.txt", "c.txt", "d.txt"] {
        r.remove_file(&RPath::new(&format!("/docs/{f}"))).unwrap();
    }
    assert_eq!(r.remove_file(&RPath::new("/docs/b.txt")).unwrap_err().kind, ErrorKind::NotFound);
    r.remove_dir(&RPath::new("/docs")).unwrap();
    assert!(!srv.root.join("docs").exists());
    assert_eq!(r.remove_dir(&RPath::root()).unwrap_err().kind, ErrorKind::InvalidPath);

    r.close();
    assert_eq!(r.list(&RPath::root()).unwrap_err().kind, ErrorKind::Closed);
    assert_eq!(r.home().unwrap_err().kind, ErrorKind::Closed);
}

#[test]
fn odd_names_round_trip() {
    let srv = Server::start(Kind::WebDav, base());
    let r = connect_rclone(&srv, srv.port);
    let names = ["a b #1 ?.txt", "100% done %20.txt", "über ✓ 日本.txt", "semi;colon+plus&amp='q'.txt"];
    r.mkdir(&RPath::new("/dir with #hash & 100%")).unwrap();
    for (i, n) in names.iter().enumerate() {
        upload(&*r, &format!("/dir with #hash & 100%/{n}"), format!("data {i}").as_bytes());
    }
    let mut got: Vec<String> = r.list(&RPath::new("/dir with #hash & 100%")).unwrap().into_iter().map(|e| e.name).collect();
    got.sort();
    let mut want: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    want.sort();
    assert_eq!(got, want);
    for (i, n) in names.iter().enumerate() {
        let p = format!("/dir with #hash & 100%/{n}");
        assert_eq!(download(&*r, &p, 0), format!("data {i}").as_bytes());
        assert_eq!(r.stat(&RPath::new(&p)).unwrap().name, *n);
    }
    let root = r.list(&RPath::root()).unwrap();
    assert_eq!(root[0].name, "dir with #hash & 100%");
    r.rename(&RPath::new(&format!("/dir with #hash & 100%/{}", names[2])), &RPath::new("/ünï cødé ✓.txt")).unwrap();
    assert_eq!(download(&*r, "/ünï cødé ✓.txt", 0), b"data 2");
}

#[test]
fn large_round_trip_and_ranged_resume() {
    const LEN: u64 = 50 * 1024 * 1024;
    let srv = Server::start(Kind::WebDav, base());
    let r = connect_rclone(&srv, srv.port);
    let mut sink = r.write(&RPath::new("/big.bin"), 0, Some(LEN)).unwrap();
    std::io::copy(&mut Noise::new(7, LEN), &mut sink).unwrap();
    sink.finish().unwrap();
    assert_eq!(r.stat(&RPath::new("/big.bin")).unwrap().size, Some(LEN));

    let n = assert_same(r.read(&RPath::new("/big.bin"), 0).unwrap(), Noise::new(7, LEN));
    assert_eq!(n, LEN);

    // Resume from the middle: the tail must match the generator past the offset.
    let offset = LEN / 2 + 12345;
    let mut expected = Noise::new(7, LEN);
    std::io::copy(&mut (&mut expected).take(offset), &mut std::io::sink()).unwrap();
    let n = assert_same(r.read(&RPath::new("/big.bin"), offset).unwrap(), expected);
    assert_eq!(n, LEN - offset);

    // Resuming at the very end reads nothing.
    assert!(download(&*r, "/big.bin", LEN).is_empty());
}

#[test]
fn dropped_connection_mid_download_is_a_network_error() {
    let srv = Server::start(Kind::WebDav, base());
    std::fs::write(srv.root.join("f.bin"), noise(3, 4 * 1024 * 1024)).unwrap();
    let proxy = FaultProxy::start(srv.addr);
    let r = connect_rclone(&srv, proxy.port);
    // Armed before the request: the proxy forwards nothing past the budget (the request is a
    // few hundred bytes of it), so the body can't all slip into socket buffers before the cut.
    proxy.cut_after(512 * 1024);
    let mut reader = r.read(&RPath::new("/f.bin"), 0).unwrap();
    let mut out = Vec::new();
    let err = reader.read_to_end(&mut out).expect_err("a cut connection must not look like EOF");
    let err = filewinch_http::from_io(err);
    assert_eq!(err.kind, ErrorKind::Network, "{err}");
    assert!(out.len() < 4 * 1024 * 1024);
    assert!(proxy.cuts() >= 1);

    // The session still works afterwards (fresh connection).
    let tail = download(&*r, "/f.bin", 4 * 1024 * 1024 - 10);
    assert_eq!(tail, noise(3, 4 * 1024 * 1024)[4 * 1024 * 1024 - 10..]);
}

#[test]
fn cancelled_upload_leaves_no_complete_file() {
    const LEN: u64 = 8 * 1024 * 1024;
    let srv = Server::start(Kind::WebDav, base());
    let r = connect_rclone(&srv, srv.port);
    for size in [Some(LEN), None] {
        let name = if size.is_some() { "sized.bin" } else { "chunked.bin" };
        let mut sink = r.write(&RPath::new(&format!("/{name}")), 0, size).unwrap();
        std::io::copy(&mut Noise::new(9, LEN / 4), &mut sink).unwrap();
        drop(sink);
        std::thread::sleep(Duration::from_millis(500));
        let on_disk = std::fs::metadata(srv.root.join(name)).map(|m| m.len()).unwrap_or(0);
        assert!(on_disk < LEN, "{name}: {on_disk} bytes on disk after cancel");
        // rclone may keep a partial file; it must never be the full content.
        if let Ok(e) = r.stat(&RPath::new(&format!("/{name}"))) {
            assert_ne!(e.size, Some(LEN));
        }
    }
}

#[test]
fn self_signed_tls_needs_trust_then_connects() {
    let srv = Server::start(Kind::WebDavs, base());
    let s = site(Protocol::WebDavs, "127.0.0.1", srv.port, &srv.user);
    let err = connect_with(&s, &srv.pass).err().expect("untrusted certificate must fail");
    assert_eq!(err.kind, ErrorKind::HostTrust, "{err}");
    let fp = err.fingerprint().expect("detail starts with the fingerprint");
    assert!(err.detail.as_deref().unwrap().starts_with(&fp));
    // The site's own trusted fingerprints are the main path.
    let mut pinned = s.clone();
    pinned.trusted_fingerprints = vec![fp.clone()];
    let r = connect_with(&pinned, &srv.pass).expect("site-trusted certificate connects");
    upload(&*r, "/pinned.txt", b"pinned");
    // The process-wide fallback still works for callers without a saved site.
    filewinch_http::trust::trust_fingerprint("127.0.0.1", srv.port, &fp);
    let r = connect_with(&s, &srv.pass).expect("connects once trusted");
    upload(&*r, "/secure.txt", b"over tls");
    assert_eq!(download(&*r, "/secure.txt", 3), b"r tls");
    filewinch_http::trust::untrust("127.0.0.1", srv.port);
}

#[test]
fn base_path_in_address_and_initial_path() {
    let srv = Server::start(Kind::WebDav, base());
    std::fs::create_dir_all(srv.root.join("users/al ice/docs")).unwrap();
    std::fs::write(srv.root.join("users/al ice/docs/x.txt"), b"x").unwrap();
    let mut s = site(Protocol::WebDav, &format!("http://127.0.0.1:{}/users/al%20ice/", srv.port), srv.port, &srv.user);
    s.port = None;
    s.initial_path = Some("docs".into());
    let r = connect_with(&s, &srv.pass).unwrap();
    assert_eq!(r.home().unwrap(), RPath::new("/docs"));
    // The scratch volume may be non-APFS, where macOS adds AppleDouble `._x` files.
    let list: Vec<_> = r.list(&RPath::new("/docs")).unwrap().into_iter().filter(|e| !e.name.starts_with("._")).collect();
    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(list[0].path, RPath::new("/docs/x.txt"));
    let root: Vec<_> = r.list(&RPath::root()).unwrap().into_iter().filter(|e| !e.name.starts_with("._")).collect();
    assert_eq!(root[0].name, "docs");

    let mut missing = s.clone();
    missing.host = format!("http://127.0.0.1:{}/no/such/base", srv.port);
    let err = connect_with(&missing, &srv.pass).err().unwrap();
    assert_eq!(err.kind, ErrorKind::NotFound, "{err}");
    assert!(err.message.contains("/no/such/base"), "{err}");
}

#[test]
fn concurrent_use_from_several_threads() {
    let srv = Server::start(Kind::WebDav, base());
    let r: std::sync::Arc<dyn Remote> = std::sync::Arc::from(connect_rclone(&srv, srv.port));
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let r = r.clone();
            std::thread::spawn(move || {
                let data = noise(i, 100_000 + i);
                upload(&*r, &format!("/t{i}.bin"), &data);
                assert_eq!(download(&*r, &format!("/t{i}.bin"), 0), data);
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(r.list(&RPath::root()).unwrap().len(), 8);
}

#[test]
fn segmented_download_64mb_over_4_connections() {
    let srv = Server::start(Kind::WebDav, base());
    let want = noise(11, 64 << 20);
    std::fs::write(srv.root.join("big.bin"), &want).unwrap();
    let r: std::sync::Arc<dyn Remote> = std::sync::Arc::from(connect_rclone(&srv, srv.port));
    let dir = filewinch_testkit::scratch_dir(base(), "dav-seg");
    let o = filewinch_testkit::segmented::download(r, &RPath::new("/big.bin"), &dir, 4);
    assert!(matches!(o.state, filewinch_engine::JobState::Done), "{o:?}");
    assert!(std::fs::read(dir.join("big.bin")).unwrap() == want, "bytes differ");
    assert!(o.max_connections >= 2 && o.ranges >= 4 && o.notes.is_empty(), "{o:?}");
    let _ = std::fs::remove_dir_all(dir);
}
