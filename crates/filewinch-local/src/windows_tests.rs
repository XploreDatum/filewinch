//! Behaviour only Windows has: the drive list, refused names, attributes and permissions.

use std::fs;
use std::io::Write;
use std::os::windows::fs::MetadataExt;
use std::path::PathBuf;

use filewinch_core::{EntryKind, ErrorKind, RPath, Remote};
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_READONLY};

use super::*;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join("filewinch-tests").join(format!("win-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // The long form of the temp folder (it may be given with 8.3 short names).
        Scratch(local_path(&rpath(&fs::canonicalize(&dir).unwrap())))
    }
    fn r(&self, rel: &str) -> RPath {
        rpath(&self.0).join(rel)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Ok(rd) = fs::read_dir(&self.0) {
            for e in rd.flatten() {
                sys::clear_readonly(&e.path());
            }
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_all(r: &LocalRemote, p: &RPath, bytes: &[u8]) {
    let mut w = r.write(p, 0, None).unwrap();
    w.write_all(bytes).unwrap();
    w.finish().unwrap();
}

#[test]
fn root_lists_drives_and_stats() {
    let r = LocalRemote::new();
    let drives = r.list(&RPath::root()).unwrap();
    assert!(!drives.is_empty());
    assert!(drives.iter().all(|d| d.kind == EntryKind::Dir && d.name.ends_with(':') && d.path.as_str() == format!("/{}", d.name)));
    let sys_drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    assert!(drives.iter().any(|d| d.name.eq_ignore_ascii_case(&sys_drive)));
    assert_eq!(r.stat(&RPath::root()).unwrap().kind, EntryKind::Dir);
    assert_eq!(r.stat(&RPath::new(&format!("/{sys_drive}"))).unwrap().kind, EntryKind::Dir);
    assert!(r.list(&RPath::new(&format!("/{sys_drive}"))).is_ok());
}

#[test]
fn the_drive_list_and_drives_cant_be_changed() {
    let r = LocalRemote::new();
    for e in [r.mkdir(&RPath::new("/Q:")).unwrap_err(), r.mkdir(&RPath::new("/x")).unwrap_err(), r.remove_dir(&RPath::new("/C:")).unwrap_err()] {
        assert_eq!(e.kind, ErrorKind::InvalidPath, "{}", e.message);
    }
    assert_eq!(r.write(&RPath::new("/C:"), 0, None).err().unwrap().kind, ErrorKind::InvalidPath);
    assert_eq!(r.list(&RPath::new("/UNC/server")).unwrap_err().kind, ErrorKind::InvalidPath);
}

#[test]
fn home_is_userprofile() {
    let home = LocalRemote::new().home().unwrap();
    assert_eq!(local_path(&home), filewinch_os::dirs::home().unwrap());
    assert!(home.components().next().unwrap().ends_with(':'));
}

#[test]
fn rpath_accepts_every_spelling() {
    let s = Scratch::new("spell");
    let r = rpath(&s.0);
    assert_eq!(rpath(&fs::canonicalize(&s.0).unwrap()), r, "verbatim prefix");
    assert_eq!(rpath(std::path::Path::new(&s.0.to_string_lossy().replace('\\', "/"))), r);
    assert_eq!(local_path(&r), s.0);
}

#[test]
fn refused_names_are_saved_under_look_alikes_and_logged() {
    let _g = crate::winpath_tests::LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ = take_renamed();
    let s = Scratch::new("names");
    let r = LocalRemote::new();
    r.mkdir(&s.r("in:box")).unwrap();
    write_all(&r, &s.r("in:box/12:30 report?.txt"), b"hello");
    assert_eq!(fs::read(s.0.join("in：box").join("12：30 report？.txt")).unwrap(), b"hello");
    // The same RPath finds it again (stat, resume, conflict check).
    let e = r.stat(&s.r("in:box/12:30 report?.txt")).unwrap();
    assert_eq!((e.name.as_str(), e.size), ("12:30 report?.txt", Some(5)));
    let mut w = r.write(&s.r("in:box/12:30 report?.txt"), 5, None).unwrap();
    w.write_all(b"!").unwrap();
    w.finish().unwrap();
    let log = take_renamed();
    assert!(log.contains(&Renamed { from: "in:box".into(), to: "in：box".into() }));
    assert!(log.contains(&Renamed { from: "12:30 report?.txt".into(), to: "12：30 report？.txt".into() }));
    assert_eq!(log.len(), 2, "a resume creates nothing");
}

#[test]
fn no_unix_permissions() {
    let s = Scratch::new("perm");
    let r = LocalRemote::new();
    assert!(!r.caps().permissions && r.caps().symlinks);
    write_all(&r, &s.r("f"), b"x");
    let e = r.stat(&s.r("f")).unwrap();
    assert_eq!((e.mode, e.owner, e.group), (None, None, None));
    let err = r.set_mode(&s.r("f"), 0o644).unwrap_err();
    assert_eq!((err.kind, err.message.as_str()), (ErrorKind::Unsupported, "Windows doesn't use Unix permissions."));
}

#[test]
fn hidden_attribute_is_reported() {
    let s = Scratch::new("hidden");
    let r = LocalRemote::new();
    write_all(&r, &s.r("secret"), b"x");
    sys::set_hidden(&s.0.join("secret"), true);
    let e = r.list(&rpath(&s.0)).unwrap().into_iter().find(|e| e.name == "secret").unwrap();
    assert!(is_hidden(&e));
    assert!(e.extra.contains(&(HIDDEN_EXTRA.0.to_owned(), HIDDEN_EXTRA.1.to_owned())));
}

#[test]
fn overwrite_keeps_the_target_visible_and_cleans_up() {
    let s = Scratch::new("overwrite");
    let r = LocalRemote::new();
    write_all(&r, &s.r("f"), b"old");
    {
        let _w = r.write(&s.r("f"), 0, None).unwrap();
        let sibling = fs::read_dir(&s.0).unwrap().flatten().find(|e| e.file_name() != "f").unwrap();
        assert!(sibling.metadata().unwrap().file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0, "the sibling is hidden");
    }
    write_all(&r, &s.r("f"), b"new");
    assert_eq!(fs::read(s.0.join("f")).unwrap(), b"new");
    assert_eq!(fs::metadata(s.0.join("f")).unwrap().file_attributes() & FILE_ATTRIBUTE_HIDDEN, 0);
    assert_eq!(fs::read_dir(&s.0).unwrap().count(), 1);
}

#[test]
fn replacing_a_read_only_file_says_why() {
    let s = Scratch::new("readonly");
    let r = LocalRemote::new();
    write_all(&r, &s.r("f"), b"old");
    let mut p = fs::metadata(s.0.join("f")).unwrap().permissions();
    p.set_readonly(true);
    fs::set_permissions(s.0.join("f"), p).unwrap();
    let mut w = r.write(&s.r("f"), 0, None).unwrap();
    w.write_all(b"new").unwrap();
    let e = w.finish().unwrap_err();
    assert_eq!(e.kind, ErrorKind::PermissionDenied);
    assert_eq!(e.message, "Couldn't replace “f”: it's open in another app or read-only.");
    assert_eq!(fs::read(s.0.join("f")).unwrap(), b"old");
    assert_eq!(fs::read_dir(&s.0).unwrap().count(), 1, "no sibling is left behind");
    assert!(fs::metadata(s.0.join("f")).unwrap().file_attributes() & FILE_ATTRIBUTE_READONLY != 0);
}

#[test]
fn mtime_can_be_set_on_read_only_files() {
    let s = Scratch::new("mtime");
    let r = LocalRemote::new();
    write_all(&r, &s.r("f"), b"x");
    let mut p = fs::metadata(s.0.join("f")).unwrap().permissions();
    p.set_readonly(true);
    fs::set_permissions(s.0.join("f"), p).unwrap();
    let when = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    r.set_mtime(&s.r("f"), when).unwrap();
    assert_eq!(r.stat(&s.r("f")).unwrap().modified, Some(when));
}

#[test]
fn symlinks_list_with_rpath_targets_when_allowed() {
    let s = Scratch::new("links");
    let r = LocalRemote::new();
    write_all(&r, &s.r("a.txt"), b"hello");
    r.mkdir(&s.r("d")).unwrap();
    match std::os::windows::fs::symlink_file(s.0.join("a.txt"), s.0.join("link")) {
        Ok(()) => {}
        // Without Developer Mode or admin rights, creating links is refused with a clear reason.
        Err(e) => {
            let err = io_error(e, "create", &s.0.join("link"));
            assert_eq!(err.kind, ErrorKind::PermissionDenied);
            assert!(err.message.contains("Developer Mode"), "{}", err.message);
            return;
        }
    }
    std::os::windows::fs::symlink_dir(s.0.join("d"), s.0.join("dlink")).unwrap();
    let e = r.stat(&s.r("link")).unwrap();
    assert_eq!((e.kind, e.size), (EntryKind::Symlink, Some(5)));
    assert_eq!(e.link_target.as_deref(), Some(s.r("a.txt").as_str()));
    // A link to a folder is deleted as a file would be; the folder stays.
    r.remove_file(&s.r("dlink")).unwrap();
    assert!(s.0.join("d").is_dir());
}

#[test]
fn rename_never_replaces_and_reports_it() {
    let s = Scratch::new("rename");
    let r = LocalRemote::new();
    write_all(&r, &s.r("a"), b"a");
    write_all(&r, &s.r("b"), b"b");
    let e = r.rename(&s.r("a"), &s.r("b")).unwrap_err();
    assert_eq!((e.kind, e.message.as_str()), (ErrorKind::AlreadyExists, "Couldn't rename: “b” already exists."));
    r.rename(&s.r("a"), &s.r("c:d")).unwrap();
    assert_eq!(fs::read(s.0.join("c：d")).unwrap(), b"a");
}
