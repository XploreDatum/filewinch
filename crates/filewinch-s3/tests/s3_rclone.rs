//! S3 against a real server: `rclone serve s3` (path style, region us-east-1) on 127.0.0.1.

use std::io::{Read, Write};
use std::path::Path;

use filewinch_core::{Auth, Connector, ErrorKind, Protocol, RPath, Remote, SecretRef, Site};
use filewinch_s3::S3Connector;
use filewinch_testkit::{FaultProxy, Kind, Server};

fn base() -> &'static Path {
    Path::new(env!("CARGO_TARGET_TMPDIR"))
}

fn site(port: u16, user: &str) -> Site {
    let mut s = Site::new("s3", "rclone", Protocol::S3);
    s.host = format!("http://127.0.0.1:{port}");
    s.user = user.into();
    s.auth = Auth::Password { secret: SecretRef("s3-secret".into()) };
    s
}

fn connect_with(port: u16, user: &str, pass: &str) -> filewinch_core::Result<Box<dyn Remote>> {
    let pass = pass.to_string();
    S3Connector.connect(&site(port, user), std::sync::Arc::new(filewinch_core::FnSecrets(move |_: &filewinch_core::SecretRef| Ok(Some(pass.clone())))))
}

fn connect(srv: &Server) -> Box<dyn Remote> {
    connect_with(srv.port, &srv.user, &srv.pass).expect("connect to rclone s3")
}

/// The scratch dir may sit on an exFAT volume, where macOS adds `._name` AppleDouble files
/// beside anything with extended attributes. They aren't part of what the tests wrote.
fn visible(entries: Vec<filewinch_core::Entry>) -> Vec<filewinch_core::Entry> {
    entries.into_iter().filter(|e| !e.name.starts_with("._")).collect()
}

/// Removes AppleDouble files so an emptied bucket folder really is empty.
fn clear_appledouble(dir: &Path) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().starts_with("._") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn pattern(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed) ^ (i >> 11) as u8).collect()
}

fn upload(r: &dyn Remote, path: &str, data: &[u8], size: Option<u64>) {
    let mut sink = r.write(&RPath::new(path), 0, size).expect("open sink");
    for chunk in data.chunks(256 * 1024 + 7) {
        sink.write_all(chunk).expect("write");
    }
    sink.finish().expect("finish upload");
}

fn download(r: &dyn Remote, path: &str, offset: u64) -> Vec<u8> {
    let mut out = Vec::new();
    r.read(&RPath::new(path), offset).expect("open read").read_to_end(&mut out).expect("read body");
    out
}

#[test]
fn buckets_folders_and_files() {
    let srv = Server::start(Kind::S3, base());
    srv.mkdir_bucket("existing");
    let r = connect(&srv);
    assert_eq!(r.home().unwrap(), RPath::root());
    let names: Vec<String> = visible(r.list(&RPath::root()).unwrap()).into_iter().map(|e| e.name).collect();
    assert_eq!(names, ["existing"]);

    // New bucket; folders come from keys (rclone's S3 server can't store empty
    // sub-folder markers, so `mkdir` below a bucket is covered by the mock tests).
    r.mkdir(&RPath::new("/fresh")).unwrap();
    assert!(r.stat(&RPath::new("/fresh")).unwrap().is_dir());
    assert_eq!(r.mkdir(&RPath::new("/fresh")).unwrap_err().kind, ErrorKind::AlreadyExists);

    let small = pattern(100_000, 1);
    upload(&*r, "/fresh/docs/small.bin", &small, Some(small.len() as u64));
    assert!(r.stat(&RPath::new("/fresh/docs")).unwrap().is_dir());
    assert_eq!(download(&*r, "/fresh/docs/small.bin", 0), small);
    assert_eq!(download(&*r, "/fresh/docs/small.bin", 99_000), &small[99_000..]);
    let st = r.stat(&RPath::new("/fresh/docs/small.bin")).unwrap();
    assert_eq!(st.size, Some(100_000));
    assert!(st.modified.is_some());
    // A single-part upload's ETag is its MD5, on HEAD and in the listing.
    let md5 = filewinch_core::Checksum { algo: filewinch_core::HashAlgo::Md5, hex: format!("{:x}", md5::compute(&small)) };
    assert_eq!(st.checksums, std::slice::from_ref(&md5));

    let list = visible(r.list(&RPath::new("/fresh")).unwrap());
    assert_eq!(list.len(), 1);
    assert!(list[0].is_dir() && list[0].name == "docs");
    let list = visible(r.list(&RPath::new("/fresh/docs")).unwrap());
    assert_eq!(list.iter().map(|e| (e.name.as_str(), e.size)).collect::<Vec<_>>(), [("small.bin", Some(100_000))]);
    assert_eq!(list[0].checksums, [md5]);

    // Not empty → refuse; empty → removed.
    assert_eq!(r.remove_dir(&RPath::new("/fresh/docs")).unwrap_err().kind, ErrorKind::InvalidPath);
    r.copy(&RPath::new("/fresh/docs/small.bin"), &RPath::new("/existing/copied.bin")).unwrap();
    assert_eq!(download(&*r, "/existing/copied.bin", 0), small);
    r.remove_file(&RPath::new("/fresh/docs/small.bin")).unwrap();
    assert_eq!(r.remove_file(&RPath::new("/fresh/docs/small.bin")).unwrap_err().kind, ErrorKind::NotFound);
    r.remove_dir(&RPath::new("/fresh/docs")).unwrap();
    assert_eq!(r.stat(&RPath::new("/fresh/docs")).unwrap_err().kind, ErrorKind::NotFound);
    clear_appledouble(&srv.root.join("fresh"));
    r.remove_dir(&RPath::new("/fresh")).unwrap();
    assert_eq!(r.stat(&RPath::new("/fresh")).unwrap_err().kind, ErrorKind::NotFound);

    // Missing things.
    assert_eq!(r.read(&RPath::new("/existing/nope.txt"), 0).err().unwrap().kind, ErrorKind::NotFound);
    assert_eq!(r.stat(&RPath::new("/existing/nope.txt")).unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(r.list(&RPath::new("/existing/nope")).unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(r.rename(&RPath::new("/existing/copied.bin"), &RPath::new("/existing/x")).unwrap_err().kind, ErrorKind::Unsupported);
}

#[test]
fn multipart_uploads_are_byte_equal() {
    let srv = Server::start(Kind::S3, base());
    srv.mkdir_bucket("big");
    let r = connect(&srv);
    // 20 MiB with a known size: 8 MiB parts streamed (8 + 8 + 4).
    let data = pattern(20 * 1024 * 1024 + 123, 7);
    upload(&*r, "/big/known.bin", &data, Some(data.len() as u64));
    assert_eq!(download(&*r, "/big/known.bin", 0), data);
    assert_eq!(std::fs::read(srv.root.join("big/known.bin")).unwrap(), data);
    // Same with an unknown size: buffered parts.
    upload(&*r, "/big/unknown.bin", &data, None);
    assert_eq!(download(&*r, "/big/unknown.bin", 5_000_000), &data[5_000_000..]);
    // Unknown size that turns out small: single PUT.
    upload(&*r, "/big/tiny.txt", b"hi", None);
    assert_eq!(download(&*r, "/big/tiny.txt", 0), b"hi");
    // Empty file.
    upload(&*r, "/big/empty", b"", Some(0));
    assert_eq!(r.stat(&RPath::new("/big/empty")).unwrap().size, Some(0));
}

#[test]
fn dropped_multipart_sink_leaves_no_object() {
    let srv = Server::start(Kind::S3, base());
    srv.mkdir_bucket("b");
    let r = connect(&srv);
    let mut sink = r.write(&RPath::new("/b/partial.bin"), 0, Some(40 * 1024 * 1024)).unwrap();
    sink.write_all(&pattern(12 * 1024 * 1024, 3)).unwrap();
    drop(sink);
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert_eq!(r.stat(&RPath::new("/b/partial.bin")).unwrap_err().kind, ErrorKind::NotFound);
    assert!(visible(r.list(&RPath::new("/b")).unwrap()).is_empty());
}

#[test]
fn dropped_single_put_sink_leaves_no_object() {
    let srv = Server::start(Kind::S3, base());
    srv.mkdir_bucket("b");
    let r = connect(&srv);
    let data = pattern(100_000, 5);
    let mut sink = r.write(&RPath::new("/b/whole.bin"), 0, Some(data.len() as u64)).unwrap();
    sink.write_all(&data).unwrap(); // every byte written, but no `finish`
    drop(sink);
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert_eq!(r.stat(&RPath::new("/b/whole.bin")).unwrap_err().kind, ErrorKind::NotFound);
    // The same upload with `finish` commits.
    upload(&*r, "/b/whole.bin", &data, Some(data.len() as u64));
    assert_eq!(download(&*r, "/b/whole.bin", 0), data);
}

#[test]
fn listing_pages_past_1000_keys() {
    let srv = Server::start(Kind::S3, base());
    let bucket = srv.mkdir_bucket("many");
    for i in 0..1100 {
        std::fs::write(bucket.join(format!("f{i:04}.txt")), b"x").unwrap();
    }
    std::fs::create_dir_all(bucket.join("sub")).unwrap();
    std::fs::write(bucket.join("sub/inner.txt"), b"y").unwrap();
    let r = connect(&srv);
    let list = visible(r.list(&RPath::new("/many")).unwrap());
    assert_eq!(list.iter().filter(|e| !e.is_dir()).count(), 1100);
    assert_eq!(list.iter().filter(|e| e.is_dir()).map(|e| e.name.as_str()).collect::<Vec<_>>(), ["sub"]);
}

#[test]
fn initial_bucket_and_wrong_secret() {
    let srv = Server::start(Kind::S3, base());
    srv.mkdir_bucket("home");
    let e = connect_with(srv.port, &srv.user, "wrong-secret").err().expect("must fail");
    assert_eq!(e.kind, ErrorKind::Auth, "{e:?}");
    assert!(e.message.contains("rejected the access key or secret"), "{}", e.message);
    let e = connect_with(srv.port, "nobody", &srv.pass).err().expect("must fail");
    assert_eq!(e.kind, ErrorKind::Auth, "{e:?}");

    let mut s = site(srv.port, &srv.user);
    s.initial_path = Some("home/sub".into());
    let pass = srv.pass.clone();
    let r = S3Connector.connect(&s, std::sync::Arc::new(filewinch_core::FnSecrets(move |_: &filewinch_core::SecretRef| Ok(Some(pass.clone()))))).unwrap();
    assert_eq!(r.home().unwrap(), RPath::new("/home/sub"));

    let mut s = site(srv.port, &srv.user);
    s.initial_path = Some("no-such-bucket".into());
    let pass = srv.pass.clone();
    let e = S3Connector.connect(&s, std::sync::Arc::new(filewinch_core::FnSecrets(move |_: &filewinch_core::SecretRef| Ok(Some(pass.clone()))))).err().unwrap();
    assert_eq!(e.kind, ErrorKind::NotFound, "{e:?}");
}

#[test]
fn cut_connection_mid_download_is_network_error() {
    let srv = Server::start(Kind::S3, base());
    let bucket = srv.mkdir_bucket("b");
    let data = pattern(8 * 1024 * 1024, 9);
    std::fs::write(bucket.join("f.bin"), &data).unwrap();
    let proxy = FaultProxy::start(srv.addr);
    let r = connect_with(proxy.port, &srv.user, &srv.pass).unwrap();
    let mut reader = r.read(&RPath::new("/b/f.bin"), 0).unwrap();
    proxy.cut_after(1024 * 1024);
    let mut out = Vec::new();
    let err = reader.read_to_end(&mut out).expect_err("the cut must surface");
    let err = filewinch_http_from_io(err);
    assert_eq!(err.kind, ErrorKind::Network, "{err:?}");
    assert!(out.len() < data.len());
    // Resume from where it stopped.
    let rest = download(&*r, "/b/f.bin", out.len() as u64);
    out.extend_from_slice(&rest);
    assert_eq!(out, data);
}

/// The core Error carried inside the io::Error (as the engine unwraps it).
fn filewinch_http_from_io(e: std::io::Error) -> filewinch_core::Error {
    match e.get_ref().and_then(|i| i.downcast_ref::<filewinch_core::Error>()) {
        Some(inner) => inner.clone(),
        None => filewinch_core::Error::from(e),
    }
}

#[test]
fn segmented_download_64mb_over_4_connections() {
    let srv = Server::start(Kind::S3, base());
    let bucket = srv.mkdir_bucket("seg");
    let want = pattern(64 << 20, 5);
    std::fs::write(bucket.join("big.bin"), &want).unwrap();
    let r: std::sync::Arc<dyn Remote> = std::sync::Arc::from(connect(&srv));
    let dir = filewinch_testkit::scratch_dir(base(), "s3-seg");
    let o = filewinch_testkit::segmented::download(r, &RPath::new("/seg/big.bin"), &dir, 4);
    assert!(matches!(o.state, filewinch_engine::JobState::Done), "{o:?}");
    assert!(std::fs::read(dir.join("big.bin")).unwrap() == want, "bytes differ");
    assert!(o.max_connections >= 2 && o.ranges >= 4 && o.notes.is_empty(), "{o:?}");
    let _ = std::fs::remove_dir_all(dir);
}

/// `cargo test --release -p filewinch-s3 --test s3_rclone -- --ignored --nocapture measure`
#[test]
#[ignore = "measurement; writes 512 MB of scratch"]
fn measure_segmented_256mb() {
    let srv = Server::start(Kind::S3, base());
    let bucket = srv.mkdir_bucket("m");
    std::fs::write(bucket.join("big.bin"), pattern(256 << 20, 3)).unwrap();
    let proxy = FaultProxy::start(([127, 0, 0, 1], srv.port).into());
    proxy.set_latency(std::time::Duration::from_millis(2));
    let dir = filewinch_testkit::scratch_dir(base(), "s3-measure");
    for (label, port) in [("direct", srv.port), ("2 ms per 16 KB proxy", proxy.port)] {
        for n in [1, 3, 8] {
            let r: std::sync::Arc<dyn Remote> = std::sync::Arc::from(connect_with(port, &srv.user, &srv.pass).unwrap());
            let o = filewinch_testkit::segmented::download(r, &RPath::new("/m/big.bin"), &dir, n);
            assert!(matches!(o.state, filewinch_engine::JobState::Done), "{o:?}");
            eprintln!("MEASURE s3 {label}: {n} connection(s): {:.2} s, {:.1} MB/s (max {} at once)", o.secs, 256.0 * 1.048_576 / o.secs, o.max_connections);
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}
