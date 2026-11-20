//! The Windows path mapping and rename log, tested on every OS (they're pure).

use std::sync::Mutex;

use filewinch_core::{EntryKind, ErrorKind, RPath};

use crate::winpath::*;

/// Serialises tests that drain the process-wide rename log.
pub(crate) static LOG_LOCK: Mutex<()> = Mutex::new(());

fn wp(p: &str) -> Option<String> {
    windows_path(&RPath::new(p))
}

fn rp(s: &str) -> String {
    rpath_from_windows(s).as_str().to_owned()
}

#[test]
fn drive_paths_map_to_windows() {
    assert_eq!(wp("/C:/Users/me/a.txt").as_deref(), Some(r"C:\Users\me\a.txt"));
    assert_eq!(wp("/C:").as_deref(), Some(r"C:\"), "a drive is its root folder");
    assert_eq!(wp("/d:/x").as_deref(), Some(r"D:\x"), "drive letters are upper-cased");
    assert_eq!(wp("/"), None, "the root lists drives and is no folder");
    assert_eq!(wp("/Users/me"), None, "a path with no drive names no place");
    assert_eq!(wp("/CC:/x"), None);
}

#[test]
fn unc_paths_map_to_windows() {
    assert_eq!(wp("/UNC/server/share/dir/a.txt").as_deref(), Some(r"\\server\share\dir\a.txt"));
    assert_eq!(wp("/UNC/server/share").as_deref(), Some(r"\\server\share\"));
    assert_eq!(wp("/unc/server/share/x").as_deref(), Some(r"\\server\share\x"));
    assert_eq!(wp("/UNC/server"), None, "a server alone isn't a folder");
    assert_eq!(wp("/UNC"), None);
    assert_eq!(wp(r"/UNC/ser\ver/share"), None, "a server name can't hold a backslash");
}

#[test]
fn refused_names_are_substituted_after_the_drive() {
    assert_eq!(wp("/C:/in:box/12:30 report?.txt").as_deref(), Some("C:\\in：box\\12：30 report？.txt"));
    assert_eq!(wp("/C:/a\\b/CON/dot.").as_deref(), Some("C:\\a＼b\\CON_\\dot．"));
    assert_eq!(wp("/UNC/srv/share/x|y").as_deref(), Some("\\\\srv\\share\\x｜y"));
    // A name Windows accepts is kept as is, so a listed name maps back to the same file.
    assert_eq!(wp("/C:/12：30 report？.txt").as_deref(), Some("C:\\12：30 report？.txt"));
}

#[test]
fn windows_paths_map_to_rpaths() {
    assert_eq!(rp(r"C:\Users\me\a.txt"), "/C:/Users/me/a.txt");
    assert_eq!(rp("C:/Users/me"), "/C:/Users/me", "forward slashes");
    assert_eq!(rp(r"c:\x\"), "/C:/x");
    assert_eq!(rp(r"C:\"), "/C:");
    assert_eq!(rp("C:"), "/C:");
    assert_eq!(rp("C:x"), "/C:/x", "drive-relative");
    assert_eq!(rp(r"\\?\C:\Users\me"), "/C:/Users/me", "verbatim, as canonicalize returns");
    assert_eq!(rp(r"\\?\UNC\server\share\x"), "/UNC/server/share/x");
    assert_eq!(rp(r"\\server\share\x"), "/UNC/server/share/x");
    assert_eq!(rp("//server/share"), "/UNC/server/share");
    assert_eq!(rp(r"\\.\C:\x"), "/C:/x", "device namespace");
    assert_eq!(rp(r"C:\a\..\b\.\c"), "/C:/b/c");
    assert_eq!(rp("/C:/x"), "/C:/x", "an RPath maps to itself");
    assert_eq!(rp(r"\x"), "/x", "no drive: under the root");
}

#[test]
fn mapping_round_trips() {
    for p in ["/C:", "/C:/Users/me/a b.txt", "/UNC/server/share", "/UNC/server/share/dir/f"] {
        let w = wp(p).unwrap();
        assert_eq!(rp(&w), p, "{w}");
    }
}

#[test]
fn volume_roots() {
    assert!(is_volume_root(&RPath::new("/C:")));
    assert!(is_volume_root(&RPath::new("/UNC/server/share")));
    assert!(!is_volume_root(&RPath::new("/C:/x")));
    assert!(!is_volume_root(&RPath::new("/UNC/server")));
    assert!(!is_volume_root(&RPath::root()));
}

#[test]
fn link_targets_read_like_rpaths() {
    assert_eq!(link_target_from_windows(r"C:\x\a.txt"), "/C:/x/a.txt");
    assert_eq!(link_target_from_windows(r"\\?\C:\x"), "/C:/x");
    assert_eq!(link_target_from_windows(r"..\docs\a.txt"), "../docs/a.txt");
    assert_eq!(link_target_from_windows("a.txt"), "a.txt");
}

#[test]
fn drive_mask_lists_drives_as_folders() {
    let drives = drive_entries(0b1101); // A:, C:, D:
    let names: Vec<_> = drives.iter().map(|e| (e.name.as_str(), e.path.as_str(), e.kind)).collect();
    assert_eq!(names, [("A:", "/A:", EntryKind::Dir), ("C:", "/C:", EntryKind::Dir), ("D:", "/D:", EntryKind::Dir)]);
    assert_eq!(drive_entries(1 << 25)[0].name, "Z:");
    assert!(drive_entries(0).is_empty());
}

#[test]
fn substitution_names_the_change() {
    assert_eq!(
        substitution(&RPath::new("/C:/d/12:30 report?.txt")),
        Some(Renamed { from: "12:30 report?.txt".into(), to: "12：30 report？.txt".into() })
    );
    assert_eq!(substitution(&RPath::new("/C:/d/fine.txt")), None);
    assert_eq!(substitution(&RPath::new("/C:")), None, "drives are never renamed");
}

#[test]
fn rename_log_drains_and_is_capped() {
    let _g = LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ = take_renamed();
    record(Renamed { from: "a:b".into(), to: "a：b".into() });
    assert_eq!(take_renamed(), [Renamed { from: "a:b".into(), to: "a：b".into() }]);
    assert!(take_renamed().is_empty(), "taking empties the log");
    for i in 0..RENAMED_CAP + 5 {
        record(Renamed { from: format!("{i}?"), to: format!("{i}？") });
    }
    let all = take_renamed();
    assert_eq!(all.len(), RENAMED_CAP);
    assert_eq!(all[0].from, "0?", "the oldest are kept");
}

#[test]
fn win32_codes_get_clear_reasons() {
    let (kind, why) = win32_reason(1314).unwrap();
    assert_eq!(kind, Some(ErrorKind::PermissionDenied));
    assert_eq!(why, "Windows needs Developer Mode or administrator rights to create symbolic links");
    assert_eq!(win32_reason(32).unwrap().1, "it's open in another app");
    assert_eq!(win32_reason(2), None);
    assert!(replace_blocked(5) && replace_blocked(32) && !replace_blocked(2));
}
