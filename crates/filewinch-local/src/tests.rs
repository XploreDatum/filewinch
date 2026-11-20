use std::fs;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

#[cfg(unix)]
use filewinch_core::EntryKind;
use filewinch_core::{ErrorKind, Protocol, RPath, Remote, Site};

use super::*;

/// A fresh scratch folder under Cargo's per-target temp dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = scratch_root().join(format!("local-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
    fn r(&self, rel: &str) -> RPath {
        rpath(&self.0.join(rel))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Restore permissions a test removed so cleanup can descend.
        #[cfg(unix)]
        if let Ok(rd) = fs::read_dir(&self.0) {
            for e in rd.flatten() {
                let _ = fs::set_permissions(e.path(), fs::Permissions::from_mode(0o755));
            }
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_all(r: &LocalRemote, p: &RPath, bytes: &[u8]) {
    let mut w = r.write(p, 0, Some(bytes.len() as u64)).unwrap();
    w.write_all(bytes).unwrap();
    w.finish().unwrap();
}

fn read_all(r: &LocalRemote, p: &RPath, offset: u64) -> Vec<u8> {
    let mut out = Vec::new();
    r.read(p, offset).unwrap().read_to_end(&mut out).unwrap();
    out
}

#[test]
fn connector_opens_this_mac() {
    let c = LocalConnector;
    assert_eq!(c.protocols(), &[Protocol::Local]);
    let remote = c.connect(&Site::new("l", "This Mac", Protocol::Local), std::sync::Arc::new(filewinch_core::MemorySecrets::new())).unwrap();
    assert!(remote.caps().real_dirs && remote.caps().resume_write);
    assert!(!remote.home().unwrap().is_root());
}

#[cfg(unix)]
#[test]
fn lists_files_folders_and_symlinks() {
    let s = Scratch::new("list");
    let r = LocalRemote::new();
    write_all(&r, &s.r("a.txt"), b"hello");
    r.mkdir(&s.r("dir")).unwrap();
    std::os::unix::fs::symlink(s.0.join("a.txt"), s.0.join("link")).unwrap();
    let mut entries = r.list(&rpath(&s.0)).unwrap();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let names: Vec<_> = entries.iter().map(|e| (e.name.as_str(), e.kind)).collect();
    assert_eq!(names, [("a.txt", EntryKind::File), ("dir", EntryKind::Dir), ("link", EntryKind::Symlink)]);
    assert_eq!(entries[0].size, Some(5));
    assert_eq!(entries[0].path, s.r("a.txt"));
    assert!(entries[0].owner.is_some(), "owner name resolves");
    assert_eq!(entries[1].size, None);
    assert_eq!(entries[2].size, Some(5), "a link to a file reports the file's size");
    assert!(entries[2].link_target.as_deref().unwrap().ends_with("a.txt"));
}

#[test]
fn list_of_missing_folder_is_not_found_with_a_human_message() {
    let s = Scratch::new("missing");
    let e = LocalRemote::new().list(&s.r("nope")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound);
    assert_eq!(e.message, "Couldn't open the folder “nope”: it doesn't exist.");
    assert!(e.detail.unwrap().contains("nope"));
}

#[cfg(unix)]
#[test]
fn unreadable_folder_is_permission_denied() {
    if unsafe { libc::geteuid() } == 0 {
        return; // root reads everything
    }
    let s = Scratch::new("perm");
    let r = LocalRemote::new();
    r.mkdir(&s.r("locked")).unwrap();
    r.set_mode(&s.r("locked"), 0o000).unwrap();
    let e = r.list(&s.r("locked")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::PermissionDenied);
    r.set_mode(&s.r("locked"), 0o755).unwrap();
}

#[test]
fn read_resumes_at_offset() {
    let s = Scratch::new("read");
    let r = LocalRemote::new();
    write_all(&r, &s.r("f"), b"0123456789");
    assert_eq!(read_all(&r, &s.r("f"), 4), b"456789");
    assert_eq!(r.read(&s.r("missing"), 0).err().unwrap().kind, ErrorKind::NotFound);
    r.mkdir(&s.r("d")).unwrap();
    assert_eq!(r.read(&s.r("d"), 0).err().unwrap().kind, ErrorKind::InvalidPath);
}

#[test]
fn write_resumes_a_partial_file() {
    let s = Scratch::new("resume");
    let r = LocalRemote::new();
    // A transfer that dies after 4 bytes leaves them in place (new file: written in place).
    {
        let mut w = r.write(&s.r("f"), 0, Some(10)).unwrap();
        w.write_all(b"0123").unwrap();
        drop(w);
    }
    assert_eq!(fs::read(s.0.join("f")).unwrap(), b"0123");
    let mut w = r.write(&s.r("f"), 4, Some(10)).unwrap();
    w.write_all(b"456789").unwrap();
    w.finish().unwrap();
    assert_eq!(fs::read(s.0.join("f")).unwrap(), b"0123456789");
    // Resuming past the end of the partial file is refused.
    let e = r.write(&s.r("f"), 99, None).err().unwrap();
    assert_eq!(e.kind, ErrorKind::InvalidPath);
}

#[test]
fn a_cancelled_overwrite_keeps_the_old_file() {
    let s = Scratch::new("overwrite");
    let r = LocalRemote::new();
    write_all(&r, &s.r("f"), b"old contents");
    #[cfg(unix)]
    fs::set_permissions(s.0.join("f"), fs::Permissions::from_mode(0o600)).unwrap();
    {
        let mut w = r.write(&s.r("f"), 0, None).unwrap();
        w.write_all(b"new").unwrap();
        // dropped without finish
    }
    assert_eq!(fs::read(s.0.join("f")).unwrap(), b"old contents");
    assert_eq!(fs::read_dir(&s.0).unwrap().count(), 1, "no hidden sibling is left behind");
    write_all(&r, &s.r("f"), b"new");
    assert_eq!(fs::read(s.0.join("f")).unwrap(), b"new");
    #[cfg(unix)]
    assert_eq!(fs::metadata(s.0.join("f")).unwrap().permissions().mode() & 0o777, 0o600, "permissions survive");
}

#[test]
fn writing_over_a_folder_is_refused() {
    let s = Scratch::new("wdir");
    let r = LocalRemote::new();
    r.mkdir(&s.r("d")).unwrap();
    assert_eq!(r.write(&s.r("d"), 0, None).err().unwrap().kind, ErrorKind::InvalidPath);
}

#[test]
fn mkdir_twice_is_already_exists() {
    let s = Scratch::new("mkdir");
    let r = LocalRemote::new();
    r.mkdir(&s.r("d")).unwrap();
    assert_eq!(r.mkdir(&s.r("d")).unwrap_err().kind, ErrorKind::AlreadyExists);
    assert_eq!(r.mkdir(&s.r("no/such/parent")).unwrap_err().kind, ErrorKind::NotFound);
}

#[test]
fn rename_never_replaces() {
    let s = Scratch::new("rename");
    let r = LocalRemote::new();
    write_all(&r, &s.r("a"), b"a");
    write_all(&r, &s.r("b"), b"b");
    let e = r.rename(&s.r("a"), &s.r("b")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::AlreadyExists);
    assert_eq!(fs::read(s.0.join("b")).unwrap(), b"b");
    r.rename(&s.r("a"), &s.r("c")).unwrap();
    assert!(!s.0.join("a").exists() && s.0.join("c").exists());
    assert_eq!(r.rename(&s.r("gone"), &s.r("x")).unwrap_err().kind, ErrorKind::NotFound);
}

#[test]
fn removes_files_and_empty_folders_only() {
    let s = Scratch::new("remove");
    let r = LocalRemote::new();
    r.mkdir(&s.r("d")).unwrap();
    write_all(&r, &s.r("d/f"), b"x");
    assert_eq!(r.remove_dir(&s.r("d")).unwrap_err().kind, ErrorKind::InvalidPath, "not empty");
    assert_eq!(r.remove_file(&s.r("d")).unwrap_err().kind, ErrorKind::InvalidPath, "a folder");
    r.remove_file(&s.r("d/f")).unwrap();
    r.remove_dir(&s.r("d")).unwrap();
    assert_eq!(r.remove_file(&s.r("d/f")).unwrap_err().kind, ErrorKind::NotFound);
}

#[test]
fn server_copy_refuses_to_replace_and_copies_files() {
    let s = Scratch::new("copy");
    let r = LocalRemote::new();
    write_all(&r, &s.r("a"), b"data");
    r.copy(&s.r("a"), &s.r("b")).unwrap();
    assert_eq!(fs::read(s.0.join("b")).unwrap(), b"data");
    assert_eq!(r.copy(&s.r("a"), &s.r("b")).unwrap_err().kind, ErrorKind::AlreadyExists);
    r.mkdir(&s.r("d")).unwrap();
    assert_eq!(r.copy(&s.r("d"), &s.r("d2")).unwrap_err().kind, ErrorKind::InvalidPath);
}

#[test]
fn sets_mtime_of_files_and_folders() {
    let s = Scratch::new("mtime");
    let r = LocalRemote::new();
    write_all(&r, &s.r("f"), b"x");
    r.mkdir(&s.r("d")).unwrap();
    let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    r.set_mtime(&s.r("f"), when).unwrap();
    r.set_mtime(&s.r("d"), when).unwrap();
    assert_eq!(r.stat(&s.r("f")).unwrap().modified, Some(when));
    assert_eq!(r.stat(&s.r("d")).unwrap().modified, Some(when));
    assert_eq!(r.set_mtime(&s.r("missing"), when).unwrap_err().kind, ErrorKind::NotFound);
}

#[cfg(unix)]
#[test]
fn sets_mode_and_reports_owner() {
    let s = Scratch::new("meta");
    let r = LocalRemote::new();
    assert!(r.caps().permissions);
    write_all(&r, &s.r("f"), b"x");
    r.set_mode(&s.r("f"), 0o640).unwrap();
    let e = r.stat(&s.r("f")).unwrap();
    assert_eq!(e.mode, Some(0o640));
    assert!(e.owner.is_some() && e.group.is_some());
    assert_eq!(r.set_mode(&s.r("missing"), 0o600).unwrap_err().kind, ErrorKind::NotFound);
}

#[test]
fn new_files_and_folders_report_created_once() {
    let s = Scratch::new("created");
    write_all(&LocalRemote::new(), &s.r("f"), b"x");
    // A new file is created; an overwrite and a resume are not.
    assert!(sink::LocalSink::open(&s.0.join("g"), 0).unwrap().created);
    assert!(!sink::LocalSink::open(&s.0.join("f"), 0).unwrap().created);
    assert!(!sink::LocalSink::open(&s.0.join("f"), 1).unwrap().created);
}

#[test]
fn hidden_means_dot_file_or_hidden_attribute() {
    let s = Scratch::new("hidden");
    let r = LocalRemote::new();
    write_all(&r, &s.r(".env"), b"x");
    write_all(&r, &s.r("plain"), b"x");
    assert!(is_hidden(&r.stat(&s.r(".env")).unwrap()));
    let mut e = r.stat(&s.r("plain")).unwrap();
    assert!(!is_hidden(&e));
    e.extra.push((HIDDEN_EXTRA.0.to_owned(), HIDDEN_EXTRA.1.to_owned()));
    assert!(is_hidden(&e));
}

#[test]
fn local_paths_round_trip() {
    let s = Scratch::new("paths");
    let r = s.r("a b/c.txt");
    assert_eq!(local_path(&r), s.0.join("a b").join("c.txt"));
    assert_eq!(rpath(&local_path(&r)), r);
}

#[test]
fn calls_after_close_fail_closed() {
    let r = LocalRemote::new();
    r.close();
    assert_eq!(r.home().unwrap_err().kind, ErrorKind::Closed);
    let e = r.list(&RPath::root()).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Closed);
    assert_eq!(e.message, format!("{}'s session was closed.", filewinch_os::Os::CURRENT.this_computer()));
}

#[test]
fn timestamps_format_as_utc() {
    assert_eq!(fsx::rfc3339(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)), "2023-11-14T22:13:20Z");
    assert_eq!(fsx::rfc3339(SystemTime::UNIX_EPOCH), "1970-01-01T00:00:00Z");
}

#[cfg(target_os = "linux")]
#[test]
fn linux_exclusive_rename_never_replaces_folders_either() {
    let s = Scratch::new("linux-rename");
    let r = LocalRemote::new();
    r.mkdir(&s.r("a")).unwrap();
    r.mkdir(&s.r("b")).unwrap();
    assert_eq!(r.rename(&s.r("a"), &s.r("b")).unwrap_err().kind, ErrorKind::AlreadyExists);
    r.rename(&s.r("a"), &s.r("c")).unwrap();
    assert!(s.0.join("c").is_dir() && !s.0.join("a").exists());
    assert!(r.stat(&s.r("c")).unwrap().owner.is_some(), "getpwuid_r resolves on Linux");
}

/// Ephemeral scratch for one test run. The build drive is exFAT (no Unix permissions, and
/// AppleDouble `._` files), so these filesystem tests use the system's temp volume (APFS).
fn scratch_root() -> PathBuf {
    std::env::temp_dir().join("filewinch-tests")
}

#[test]
fn positional_writes_from_threads_and_keeps_bytes_when_reopened() {
    let s = Scratch::new("positional");
    let r = LocalRemote::new();
    let p = s.r(".big.bin.filewinch-part");
    let size = 3 * 1024 * 1024 + 17;
    let want: Vec<u8> = (0..size).map(|i| (i * 31 % 251) as u8).collect();
    let sink = r.open_positional(&p, size as u64).unwrap();
    assert_eq!(fs::metadata(s.0.join(".big.bin.filewinch-part")).unwrap().len(), size as u64, "length set up front");
    let third = size / 3;
    std::thread::scope(|sc| {
        // Only the first two thirds now; the rest after reopening.
        for (a, b) in [(0, third), (third, 2 * third)] {
            let (sink, want) = (&sink, &want);
            sc.spawn(move || {
                for at in (a..b).step_by(65_536) {
                    let end = (at + 65_536).min(b);
                    sink.write_at(at as u64, &want[at..end]).unwrap();
                }
            });
        }
    });
    sink.sync().unwrap();
    drop(sink); // abort: the bytes stay
    let sink = r.open_positional(&p, size as u64).unwrap();
    sink.write_at(2 * third as u64, &want[2 * third..]).unwrap();
    sink.finish().unwrap();
    assert_eq!(fs::read(s.0.join(".big.bin.filewinch-part")).unwrap(), want);
}

#[test]
fn positional_refuses_a_folder_and_ranged_reads_stop_at_len() {
    let s = Scratch::new("positional-folder");
    let r = LocalRemote::new();
    fs::create_dir(s.0.join("d")).unwrap();
    assert_eq!(r.open_positional(&s.r("d"), 10).err().unwrap().kind, ErrorKind::InvalidPath);
    fs::write(s.0.join("f"), b"0123456789").unwrap();
    let mut out = String::new();
    r.read_range(&s.r("f"), 3, 4).unwrap().read_to_string(&mut out).unwrap();
    assert_eq!(out, "3456");
}
