//! Google Drive backend against a stateful mock Drive v3.

mod mock_gdrive;
mod support;

use std::io::{Read, Write};
use std::sync::atomic::Ordering;
use std::time::{Duration, UNIX_EPOCH};

use filewinch_cloud::{GoogleDriveConnector, Provider, Tuning};
use filewinch_core::{Checksum, Connector, EntryKind, ErrorKind, HashAlgo, Protocol, RPath, Remote};
use mock_gdrive::{DOC_MIME, FOLDER, FakeDrive, ROOT, SHORTCUT_MIME, drive_error};
use support::Reply;

const KIB: usize = 1024;

fn tuning() -> Tuning {
    Tuning {
        simple_max: 300 * KIB as u64,
        chunk: 256 * KIB,
        page_size: 2,
        ..Tuning::for_provider(Provider::GoogleDrive)
    }
}

fn connect(fake: &FakeDrive) -> Box<dyn Remote> {
    GoogleDriveConnector::with_options(support::options(&fake.mock, tuning()))
        .connect(&support::site(Protocol::GoogleDrive), support::secrets())
        .unwrap()
}

fn p(s: &str) -> RPath {
    RPath::new(s)
}

fn names(r: &dyn Remote, dir: &str) -> Vec<String> {
    r.list(&p(dir))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect()
}

fn read_all(r: &dyn Remote, path: &str, offset: u64) -> Vec<u8> {
    let mut out = Vec::new();
    r.read(&p(path), offset)
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    out
}

fn upload(r: &dyn Remote, path: &str, data: &[u8], size: Option<u64>) {
    let mut w = r.write(&p(path), 0, size).unwrap();
    for c in data.chunks(10_000) {
        w.write_all(c).unwrap();
    }
    w.finish().unwrap();
}

#[test]
fn layout_home_and_connector_basics() {
    let fake = FakeDrive::start();
    let r = connect(&fake);
    assert_eq!(
        GoogleDriveConnector::new().protocols(),
        &[Protocol::GoogleDrive]
    );
    assert_eq!(r.home().unwrap(), p("/My Drive"));
    assert_eq!(
        names(&*r, "/"),
        ["My Drive", "Shared drives", "Shared with me"]
    );
    assert!(r.stat(&p("/My Drive")).unwrap().is_dir());
    let caps = r.caps();
    assert!(
        caps.resume_read
            && caps.server_copy
            && caps.server_rename
            && caps.set_mtime
            && !caps.resume_write
    );
    assert_eq!(
        r.stat(&p("/Elsewhere")).unwrap_err().kind,
        ErrorKind::NotFound
    );
}

#[test]
fn pagination_and_nested_paths() {
    let fake = FakeDrive::start();
    let a = fake.add_folder("a", ROOT);
    let b = fake.add_folder("b", &a);
    for i in 0..5 {
        fake.add_file(&format!("f{i}.txt"), &b, format!("file {i}").as_bytes());
    }
    let r = connect(&fake);
    assert_eq!(
        names(&*r, "/My Drive/a/b"),
        ["f0.txt", "f1.txt", "f2.txt", "f3.txt", "f4.txt"]
    );
    let pages = fake
        .mock
        .requests()
        .iter()
        .filter(|q| q.path == "/api/files" && q.query.contains_key("pageToken"))
        .count();
    assert!(
        pages >= 2,
        "page size 2 must paginate, saw {pages} follow-up pages"
    );
    assert_eq!(read_all(&*r, "/My Drive/a/b/f3.txt", 0), b"file 3");
    let e = r.stat(&p("/My Drive/a/b/f3.txt")).unwrap();
    assert_eq!((e.kind, e.size), (EntryKind::File, Some(6)));
    assert_eq!(
        r.stat(&p("/My Drive/a/missing/x")).unwrap_err().kind,
        ErrorKind::NotFound
    );
    // A file in the middle of a path is not a folder.
    assert_eq!(
        r.stat(&p("/My Drive/a/b/f3.txt/x")).unwrap_err().kind,
        ErrorKind::NotFound
    );
}

#[test]
fn duplicate_names_are_stable_and_resolve_to_the_right_item() {
    let fake = FakeDrive::start();
    fake.add(
        "report.pdf",
        ROOT,
        "application/pdf",
        b"third",
        "2024-03-01T00:00:00.000Z",
    );
    fake.add(
        "report.pdf",
        ROOT,
        "application/pdf",
        b"first",
        "2024-01-01T00:00:00.000Z",
    );
    fake.add(
        "report.pdf",
        ROOT,
        "application/pdf",
        b"second",
        "2024-02-01T00:00:00.000Z",
    );
    let r = connect(&fake);
    let first = names(&*r, "/My Drive");
    assert_eq!(first, ["report (2).pdf", "report (3).pdf", "report.pdf"]);
    assert_eq!(names(&*r, "/My Drive"), first, "stable between listings");
    assert_eq!(read_all(&*r, "/My Drive/report.pdf", 0), b"first");
    assert_eq!(read_all(&*r, "/My Drive/report (2).pdf", 0), b"second");
    assert_eq!(read_all(&*r, "/My Drive/report (3).pdf", 0), b"third");
}

#[test]
fn google_docs_are_listed_and_exported() {
    let fake = FakeDrive::start();
    fake.add("Plan", ROOT, DOC_MIME, b"", "2024-01-01T00:00:00.000Z");
    fake.add(
        "Survey",
        ROOT,
        "application/vnd.google-apps.form",
        b"",
        "2024-01-01T00:00:00.000Z",
    );
    let r = connect(&fake);
    let list = r.list(&p("/My Drive")).unwrap();
    assert_eq!(
        list.len(),
        1,
        "forms have nothing to download and are left out"
    );
    assert_eq!((list[0].name.as_str(), list[0].size), ("Plan.docx", None));
    let want =
        "EXPORT application/vnd.openxmlformats-officedocument.wordprocessingml.document OF Plan";
    assert_eq!(read_all(&*r, "/My Drive/Plan.docx", 0), want.as_bytes());
    assert_eq!(
        read_all(&*r, "/My Drive/Plan.docx", 7),
        &want.as_bytes()[7..],
        "offset skips into the export"
    );
}

#[test]
fn uploads_below_and_above_the_session_threshold_are_byte_equal() {
    let fake = FakeDrive::start();
    let r = connect(&fake);
    let small = support::bytes(100 * KIB, 1);
    upload(&*r, "/My Drive/small.bin", &small, Some(small.len() as u64));
    assert_eq!(fake.named("small.bin")[0].data, small);
    assert_eq!(
        fake.mock
            .requests()
            .iter()
            .filter(|q| q.query.get("uploadType").map(String::as_str) == Some("multipart"))
            .count(),
        1
    );

    // Unknown size, 1 MiB + change: session opens once the buffer passes 300 KiB; Drive
    // only half-accepts one chunk, so the remainder must be re-sent.
    fake.state.lock().unwrap().partial_ack_once = true;
    let big = support::bytes(1024 * KIB + 123, 2);
    upload(&*r, "/My Drive/big.bin", &big, None);
    assert_eq!(fake.named("big.bin")[0].data, big);
    let puts: Vec<_> = fake
        .mock
        .requests()
        .into_iter()
        .filter(|q| q.method == "PUT")
        .collect();
    assert!(
        puts.len() >= 5,
        "streamed in 256 KiB chunks plus one resend, saw {}",
        puts.len()
    );
    assert!(
        puts.iter()
            .take(puts.len() - 1)
            .all(|q| q.header("Content-Range").unwrap().ends_with("/*"))
    );
    assert!(
        puts.last()
            .unwrap()
            .header("Content-Range")
            .unwrap()
            .ends_with(&format!("/{}", big.len()))
    );
    assert_eq!(
        r.stat(&p("/My Drive/big.bin")).unwrap().size,
        Some(big.len() as u64)
    );
}

#[test]
fn overwrite_updates_in_place() {
    let fake = FakeDrive::start();
    let id = fake.add_file("notes.txt", ROOT, b"old");
    let r = connect(&fake);
    upload(&*r, "/My Drive/notes.txt", b"new contents", None);
    let big = support::bytes(400 * KIB, 3);
    upload(&*r, "/My Drive/notes.txt", &big, Some(big.len() as u64));
    let files = fake.named("notes.txt");
    assert_eq!(files.len(), 1, "no duplicate created");
    assert_eq!(files[0].id, id);
    assert_eq!(files[0].data, big);
}

#[test]
fn dropping_an_unfinished_upload_cancels_the_session() {
    let fake = FakeDrive::start();
    let r = connect(&fake);
    let mut w = r
        .write(&p("/My Drive/partial.bin"), 0, Some(1024 * 1024))
        .unwrap();
    w.write_all(&support::bytes(600 * KIB, 4)).unwrap();
    drop(w);
    let st = fake.state.lock().unwrap();
    assert!(st.uploads.values().all(|u| u.cancelled));
    assert_eq!(st.uploads.len(), 1);
    drop(st);
    assert_eq!(
        fake.mock
            .requests()
            .iter()
            .filter(|q| q.method == "DELETE")
            .count(),
        1
    );
    assert!(fake.named("partial.bin").is_empty());

    // A short upload fails at finish and also cancels.
    let mut w = r
        .write(&p("/My Drive/short.bin"), 0, Some(1024 * 1024))
        .unwrap();
    w.write_all(&[1; 10]).unwrap();
    assert!(w.finish().is_err());
    assert!(fake.named("short.bin").is_empty());
}

#[test]
fn ranged_download() {
    let fake = FakeDrive::start();
    let data = support::bytes(5000, 5);
    fake.add_file("data.bin", ROOT, &data);
    let r = connect(&fake);
    assert_eq!(read_all(&*r, "/My Drive/data.bin", 1000), &data[1000..]);
    let last = fake
        .mock
        .requests()
        .into_iter()
        .rfind(|q| q.query.contains_key("alt"))
        .unwrap();
    assert_eq!(last.header("Range"), Some("bytes=1000-"));
}

#[test]
fn refreshes_the_token_on_401() {
    let fake = FakeDrive::start();
    fake.add_file("x.txt", ROOT, b"x");
    let r = connect(&fake);
    assert_eq!(names(&*r, "/My Drive"), ["x.txt"]);
    fake.tokens.expire_all();
    assert_eq!(read_all(&*r, "/My Drive/x.txt", 0), b"x");
    assert_eq!(fake.tokens.refreshes.load(Ordering::SeqCst), 2);
}

#[test]
fn rate_limits_and_quota() {
    let fake = FakeDrive::start();
    fake.add_file("x.txt", ROOT, b"x");
    let r = connect(&fake);
    r.stat(&p("/My Drive/x.txt")).unwrap();

    fake.set_hook(|q| {
        (q.path == "/api/files").then(|| {
            Reply::json(
                429,
                serde_json::json!({"error": {"code": 429, "message": "Too many"}}),
            )
            .header("Retry-After", "7")
        })
    });
    let e = r.list(&p("/My Drive")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::RateLimited);
    assert_eq!(e.retry_after, Some(Duration::from_secs(7)));

    fake.set_hook(|q| {
        (q.path == "/api/files")
            .then(|| drive_error(403, "userRateLimitExceeded", "User rate limit exceeded."))
    });
    let e = r.list(&p("/My Drive")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::RateLimited);
    assert!(e.kind.is_transient());

    fake.set_hook(|q| {
        q.path.starts_with("/upload").then(|| {
            drive_error(
                403,
                "storageQuotaExceeded",
                "The user's Drive storage quota has been exceeded.",
            )
        })
    });
    let mut w = r.write(&p("/My Drive/new.txt"), 0, None).unwrap();
    w.write_all(b"hello").unwrap();
    assert_eq!(w.finish().unwrap_err().kind, ErrorKind::NoSpace);
}

#[test]
fn not_found_and_permission_errors() {
    let fake = FakeDrive::start();
    let id = fake.add_file("secret.txt", ROOT, b"s");
    let r = connect(&fake);
    let e = r.stat(&p("/My Drive/nope.txt")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound);
    assert!(e.message.contains("/My Drive/nope.txt"), "{e}");
    let path = format!("/api/files/{id}");
    fake.set_hook(move |q| {
        (q.path == path).then(|| {
            drive_error(
                403,
                "insufficientFilePermissions",
                "The user does not have sufficient permissions for this file.",
            )
        })
    });
    let e = r.read(&p("/My Drive/secret.txt"), 0).err().unwrap();
    assert_eq!(e.kind, ErrorKind::PermissionDenied);
    // Item deleted on the server after it was cached.
    fake.clear_hook();
    fake.state.lock().unwrap().files.remove(&id);
    assert_eq!(
        r.read(&p("/My Drive/secret.txt"), 0).err().unwrap().kind,
        ErrorKind::NotFound
    );
}

#[test]
fn shared_drives_and_shared_with_me() {
    let fake = FakeDrive::start();
    fake.add_drive("drive-1", "Team");
    fake.add_drive("drive-2", "Design");
    fake.add_drive("drive-3", "Ops");
    let folder = fake.add_folder("Specs", "drive-1");
    fake.add_file("spec.md", &folder, b"# spec");
    let shared = fake.add_file("from-bo.txt", "someone-else", b"hi");
    fake.set_shared(&shared);
    let r = connect(&fake);

    assert_eq!(names(&*r, "/Shared drives"), ["Design", "Ops", "Team"]);
    assert_eq!(names(&*r, "/Shared drives/Team"), ["Specs"]);
    assert_eq!(
        read_all(&*r, "/Shared drives/Team/Specs/spec.md", 0),
        b"# spec"
    );
    upload(&*r, "/Shared drives/Team/Specs/new.md", b"new", None);
    assert_eq!(fake.named("new.md")[0].drive_id.as_deref(), Some("drive-1"));

    assert_eq!(names(&*r, "/Shared with me"), ["from-bo.txt"]);
    assert_eq!(read_all(&*r, "/Shared with me/from-bo.txt", 0), b"hi");
    for e in [
        r.write(&p("/Shared with me/x.txt"), 0, None).err().unwrap(),
        r.mkdir(&p("/Shared with me/dir")).unwrap_err(),
        r.remove_file(&p("/Shared with me/from-bo.txt"))
            .unwrap_err(),
        r.rename(&p("/Shared with me/from-bo.txt"), &p("/My Drive/b.txt"))
            .unwrap_err(),
        r.mkdir(&p("/Shared drives/New")).unwrap_err(),
        r.remove_dir(&p("/My Drive")).unwrap_err(),
    ] {
        assert_eq!(e.kind, ErrorKind::PermissionDenied, "{e}");
    }
    // Copying out of Shared with me is fine.
    r.copy(&p("/Shared with me/from-bo.txt"), &p("/My Drive/copy.txt"))
        .unwrap();
    assert_eq!(read_all(&*r, "/My Drive/copy.txt", 0), b"hi");
}

#[test]
fn delete_trashes_and_dirs_must_be_empty() {
    let fake = FakeDrive::start();
    let f = fake.add_file("old.txt", ROOT, b"o");
    let d = fake.add_folder("full", ROOT);
    fake.add_file("inside.txt", &d, b"i");
    let empty = fake.add_folder("empty", ROOT);
    let r = connect(&fake);
    r.remove_file(&p("/My Drive/old.txt")).unwrap();
    assert!(fake.file(&f).trashed, "moved to trash, not deleted");
    assert_eq!(
        r.stat(&p("/My Drive/old.txt")).unwrap_err().kind,
        ErrorKind::NotFound
    );
    assert_eq!(
        r.remove_dir(&p("/My Drive/full")).unwrap_err().kind,
        ErrorKind::InvalidPath
    );
    assert_eq!(
        r.remove_file(&p("/My Drive/full")).unwrap_err().kind,
        ErrorKind::InvalidPath
    );
    r.remove_dir(&p("/My Drive/empty")).unwrap();
    assert!(fake.file(&empty).trashed);
}

#[test]
fn mkdir_rename_copy_and_mtime() {
    let fake = FakeDrive::start();
    let f = fake.add_file("a.txt", ROOT, b"aaa");
    fake.add("Plan", ROOT, DOC_MIME, b"", "2024-01-01T00:00:00.000Z");
    let r = connect(&fake);
    r.mkdir(&p("/My Drive/dir")).unwrap();
    assert_eq!(
        r.mkdir(&p("/My Drive/dir")).unwrap_err().kind,
        ErrorKind::AlreadyExists
    );
    assert!(r.stat(&p("/My Drive/dir")).unwrap().is_dir());

    r.rename(&p("/My Drive/a.txt"), &p("/My Drive/dir/b.txt"))
        .unwrap();
    let moved = fake.file(&f);
    assert_eq!(moved.name, "b.txt");
    assert_eq!(moved.parents.len(), 1);
    assert_ne!(moved.parents[0], ROOT);
    assert_eq!(read_all(&*r, "/My Drive/dir/b.txt", 0), b"aaa");
    assert_eq!(
        r.stat(&p("/My Drive/a.txt")).unwrap_err().kind,
        ErrorKind::NotFound
    );

    r.copy(&p("/My Drive/dir/b.txt"), &p("/My Drive/c.txt"))
        .unwrap();
    assert_eq!(read_all(&*r, "/My Drive/c.txt", 0), b"aaa");
    assert_eq!(
        r.copy(&p("/My Drive/dir"), &p("/My Drive/dir2"))
            .unwrap_err()
            .kind,
        ErrorKind::Unsupported
    );
    assert_eq!(
        r.copy(&p("/My Drive/c.txt"), &p("/My Drive/dir/b.txt"))
            .unwrap_err()
            .kind,
        ErrorKind::AlreadyExists
    );

    // Renaming a Google Doc keeps it a Doc (export extension isn't stored).
    r.rename(&p("/My Drive/Plan.docx"), &p("/My Drive/Roadmap.docx"))
        .unwrap();
    assert_eq!(fake.named("Roadmap").len(), 1);
    assert!(names(&*r, "/My Drive").contains(&"Roadmap.docx".to_string()));

    let t = UNIX_EPOCH + Duration::from_secs(1_709_634_030);
    r.set_mtime(&p("/My Drive/c.txt"), t).unwrap();
    assert_eq!(fake.named("c.txt")[0].modified, "2024-03-05T10:20:30Z");
    assert_eq!(r.stat(&p("/My Drive/c.txt")).unwrap().modified, Some(t));
}

#[test]
fn close_makes_later_calls_fail() {
    let fake = FakeDrive::start();
    let r = connect(&fake);
    r.close();
    assert_eq!(r.list(&p("/My Drive")).unwrap_err().kind, ErrorKind::Closed);
}

#[test]
fn revoked_refresh_token_fails_at_connect() {
    let fake = FakeDrive::start();
    let e = GoogleDriveConnector::with_options(support::options(&fake.mock, tuning()))
        .connect(&support::site(Protocol::GoogleDrive), support::fixed(Some("revoked")))
        .err()
        .unwrap();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.message.contains("expired or was revoked"), "{e}");
}

fn ck(algo: HashAlgo, hex: &str) -> Checksum {
    Checksum::new(algo, hex).unwrap()
}

#[test]
fn checksums_come_through_on_list_and_stat() {
    let fake = FakeDrive::start();
    fake.add_file("data.bin", ROOT, b"checksum me");
    fake.add_folder("dir", ROOT);
    fake.add("Plan", ROOT, DOC_MIME, b"", "2024-01-01T00:00:00.000Z");
    let r = connect(&fake);
    let want = vec![
        ck(HashAlgo::Md5, &mock_gdrive::md5_hex(b"checksum me")),
        ck(HashAlgo::Sha1, &mock_gdrive::sha1_hex(b"checksum me")),
        ck(HashAlgo::Sha256, &mock_gdrive::sha256_hex(b"checksum me")),
    ];
    let list = r.list(&p("/My Drive")).unwrap();
    let by = |n: &str| list.iter().find(|e| e.name == n).unwrap().clone();
    assert_eq!(by("data.bin").checksums, want);
    assert!(by("dir").checksums.is_empty(), "folders have no hash");
    assert!(
        by("Plan.docx").checksums.is_empty(),
        "a Google Doc reads as an export: no stored hash"
    );
    assert_eq!(r.stat(&p("/My Drive/data.bin")).unwrap().checksums, want);
    assert!(r.stat(&p("/My Drive")).unwrap().checksums.is_empty());
    // Every listing asks Drive for all three hashes.
    let lists: Vec<_> = fake
        .mock
        .requests()
        .into_iter()
        .filter(|q| q.method == "GET" && q.path == "/api/files")
        .collect();
    assert!(!lists.is_empty());
    for q in lists {
        let fields = &q.query["fields"];
        for f in ["md5Checksum", "sha1Checksum", "sha256Checksum"] {
            assert!(fields.contains(f), "{f} missing from fields={fields}");
        }
    }
}

#[test]
fn malformed_or_misplaced_hashes_are_dropped() {
    let fake = FakeDrive::start();
    let good = "A".repeat(64);
    let files = serde_json::json!({"files": [
        // Short MD5, non-hex SHA-1, upper-case SHA-256 (valid: lower-cased).
        {"id": "x1", "name": "odd.bin", "mimeType": "application/octet-stream", "size": "3",
         "md5Checksum": "abc", "sha1Checksum": "z".repeat(40), "sha256Checksum": good},
        {"id": "x2", "name": "empty.bin", "mimeType": "application/octet-stream", "size": "0",
         "md5Checksum": "", "sha1Checksum": " "},
        // A server that sent a hash for a Doc or a folder is not believed.
        {"id": "x3", "name": "Plan", "mimeType": DOC_MIME, "md5Checksum": "0".repeat(32)},
        {"id": "x4", "name": "dir", "mimeType": FOLDER, "md5Checksum": "0".repeat(32)},
        {"id": "x5", "name": "Link", "mimeType": SHORTCUT_MIME, "md5Checksum": "0".repeat(32),
         "shortcutDetails": {"targetId": "x1"}},
    ]});
    fake.set_hook(move |q| {
        (q.method == "GET" && q.path == "/api/files").then(|| Reply::json(200, files.clone()))
    });
    let r = connect(&fake);
    let list = r.list(&p("/My Drive")).unwrap();
    let names: Vec<_> = list.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["Plan.docx", "dir", "empty.bin", "odd.bin"]);
    let by = |n: &str| list.iter().find(|e| e.name == n).unwrap();
    assert_eq!(
        by("odd.bin").checksums,
        [ck(HashAlgo::Sha256, &"a".repeat(64))]
    );
    for n in ["empty.bin", "Plan.docx", "dir"] {
        assert!(by(n).checksums.is_empty(), "{n}");
    }
}

#[test]
fn shortcuts_are_hidden_and_their_targets_never_touched() {
    let fake = FakeDrive::start();
    let target = fake.add_folder("Projects", ROOT);
    let inside = fake.add_file("keep.txt", &target, b"precious");
    let elsewhere = fake.add_folder("Elsewhere", ROOT);
    let link = fake.add(
        "Projects link",
        &elsewhere,
        SHORTCUT_MIME,
        target.as_bytes(),
        "2024-01-01T00:00:00.000Z",
    );
    let r = connect(&fake);
    assert!(
        r.list(&p("/My Drive/Elsewhere")).unwrap().is_empty(),
        "a shortcut has no bytes of its own and isn't listed"
    );
    for res in [
        r.remove_file(&p("/My Drive/Elsewhere/Projects link")),
        r.remove_dir(&p("/My Drive/Elsewhere/Projects link")),
    ] {
        assert_eq!(res.unwrap_err().kind, ErrorKind::NotFound);
    }
    assert_eq!(
        r.list(&p("/My Drive/Elsewhere/Projects link"))
            .unwrap_err()
            .kind,
        ErrorKind::NotFound,
        "browsing never follows a shortcut into its target"
    );
    // The folder holding only a shortcut isn't removed out from under it either: Drive
    // would trash the shortcut with it, so the emptiness check counts hidden items.
    assert_eq!(
        r.remove_dir(&p("/My Drive/Elsewhere")).unwrap_err().kind,
        ErrorKind::InvalidPath
    );
    for id in [&target, &inside, &link, &elsewhere] {
        assert!(!fake.file(id).trashed, "{id} was trashed");
    }
    let touched: Vec<_> = fake
        .mock
        .requests()
        .into_iter()
        .filter(|q| q.method != "GET" && q.path.starts_with("/api"))
        .map(|q| q.path)
        .collect();
    assert!(touched.is_empty(), "no writes at all: {touched:?}");
    assert_eq!(read_all(&*r, "/My Drive/Projects/keep.txt", 0), b"precious");
}
