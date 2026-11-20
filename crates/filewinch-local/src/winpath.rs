//! Windows paths as [`RPath`]s, and the log of names changed for Windows. Everything here is
//! pure (no Windows calls), so the mapping is tested on every OS.
//!
//! The local backend's [`RPath`] stays `/`-separated. On Windows its first component is a
//! drive or `UNC`:
//!
//! | RPath | Windows path |
//! | --- | --- |
//! | `/` | none: the root lists the drives |
//! | `/C:` | `C:\` |
//! | `/C:/Users/me/a.txt` | `C:\Users\me\a.txt` |
//! | `/UNC/server/share/a.txt` | `\\server\share\a.txt` |
//!
//! Every component after the drive (or share) goes through [`windows_safe`], so a remote
//! `12:30 report?.txt` is saved as `12：30 report？.txt`, and a later stat, resume or conflict
//! check of the same RPath finds that file. When `write` or `mkdir` creates an item under a
//! substituted name it is recorded for [`take_renamed`], so the app can say so.

use std::sync::{Mutex, PoisonError};

use filewinch_core::{Entry, EntryKind, ErrorKind, RPath};
use filewinch_os::Os;
use filewinch_os::names::{check, windows_safe};

const UNC: &str = "UNC";

/// A remote name that was saved on this computer under another name, because Windows
/// refuses the original (see `filewinch_os::names::windows_safe`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Renamed {
    /// The name as it is on the remote: `12:30 report?.txt`.
    pub from: String,
    /// The name it was saved under: `12：30 report？.txt`.
    pub to: String,
}

/// At most this many renames are kept between two [`take_renamed`] calls; later ones are
/// dropped (the first ones are enough for the note the app shows).
pub const RENAMED_CAP: usize = 1000;

static RENAMED: Mutex<Vec<Renamed>> = Mutex::new(Vec::new());

/// The items saved under a Windows-safe name since the last call, oldest first, and empties
/// the log. Always empty on macOS and Linux, which accept every remote name.
pub fn take_renamed() -> Vec<Renamed> {
    std::mem::take(&mut *RENAMED.lock().unwrap_or_else(PoisonError::into_inner))
}

pub(crate) fn record(r: Renamed) {
    let mut log = RENAMED.lock().unwrap_or_else(PoisonError::into_inner);
    if log.len() < RENAMED_CAP {
        log.push(r);
    }
}

/// The rename `path`'s last component gets on Windows, if any.
pub(crate) fn substitution(path: &RPath) -> Option<Renamed> {
    if is_volume_root(path) {
        return None;
    }
    let from = path.name();
    let to = windows_safe(from);
    (to != from).then(|| Renamed { from: from.to_owned(), to: to.into_owned() })
}

fn is_drive(c: &str) -> bool {
    let b = c.as_bytes();
    b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// The Windows path for `p`, or `None` when `p` names no place on disk: the root (the drive
/// list), `/UNC`, `/UNC/server`, or a first component that is neither a drive nor `UNC`.
pub fn windows_path(p: &RPath) -> Option<String> {
    let parts: Vec<&str> = p.components().collect();
    let first = *parts.first()?;
    let (mut out, rest) = if is_drive(first) {
        (format!("{}\\", first.to_ascii_uppercase()), &parts[1..])
    } else if first.eq_ignore_ascii_case(UNC) && parts.len() >= 3 {
        let (server, share) = (parts[1], parts[2]);
        if check(Os::Windows, server).is_some() || check(Os::Windows, share).is_some() {
            return None;
        }
        (format!("\\\\{server}\\{share}\\"), &parts[3..])
    } else {
        return None;
    };
    let names: Vec<String> = rest.iter().map(|c| windows_safe(c).into_owned()).collect();
    out.push_str(&names.join("\\"));
    Some(out)
}

/// True for a drive (`/C:`) or a network share (`/UNC/server/share`): places that exist on
/// disk but can't be created, renamed or deleted.
pub fn is_volume_root(p: &RPath) -> bool {
    let parts: Vec<&str> = p.components().collect();
    match parts.first() {
        Some(d) if is_drive(d) => parts.len() == 1,
        Some(u) if u.eq_ignore_ascii_case(UNC) => parts.len() == 3,
        _ => false,
    }
}

/// The RPath for a Windows path: `C:\x`, `C:/x`, `c:x`, verbatim `\\?\C:\x` (what
/// `canonicalize` returns), `\\server\share\x` and `\\?\UNC\server\share\x`. A path with no
/// drive (`\x`, `x`) maps under the root, where it names no place on disk.
pub fn rpath_from_windows(s: &str) -> RPath {
    let s = s.replace('/', "\\");
    let head = |n: usize| s.get(..n).unwrap_or("");
    let (prefix, rest) = if head(8).eq_ignore_ascii_case(r"\\?\UNC\") || head(8).eq_ignore_ascii_case(r"\\.\UNC\") {
        ("/UNC/", &s[8..])
    } else if head(4) == r"\\?\" || head(4) == r"\\.\" {
        ("/", &s[4..])
    } else if head(2) == r"\\" {
        ("/UNC/", &s[2..])
    } else {
        ("/", s.as_str())
    };
    let p = RPath::new(&format!("{prefix}{}", rest.replace('\\', "/")));
    // Normalise the drive: `c:` → `C:`, and a drive-relative `C:x` → `C:/x`.
    let mut parts: Vec<String> = p.components().map(str::to_owned).collect();
    if let Some(first) = parts.first().cloned()
        && first.get(..2).is_some_and(is_drive)
    {
        parts[0] = first[..2].to_ascii_uppercase();
        if first.len() > 2 {
            parts.insert(1, first[2..].to_owned());
        }
    }
    if parts.first().is_some_and(|f| f.eq_ignore_ascii_case(UNC)) {
        parts[0] = UNC.to_owned();
    }
    RPath::new(&parts.join("/"))
}

/// A symlink's target as the engine reads it: an absolute Windows target becomes its RPath
/// (`C:\x\a.txt` → `/C:/x/a.txt`), a relative one keeps its shape with `/` separators.
pub(crate) fn link_target_from_windows(t: &str) -> String {
    let b = t.as_bytes();
    let absolute = t.starts_with(['\\', '/']) || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':');
    if absolute { rpath_from_windows(t).as_str().to_owned() } else { t.replace('\\', "/") }
}

/// The drives in a `GetLogicalDrives` bit mask (bit 0 is `A:`) as folder entries.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn drive_entries(mask: u32) -> Vec<Entry> {
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| {
            let name = format!("{}:", char::from(b'A' + i));
            let path = RPath::root().join(&name);
            dir_entry(name, path)
        })
        .collect()
}

/// A folder entry with nothing known but its name.
pub(crate) fn dir_entry(name: String, path: RPath) -> Entry {
    Entry {
        name,
        path,
        kind: EntryKind::Dir,
        size: None,
        modified: None,
        mode: None,
        owner: None,
        group: None,
        link_target: None,
        extra: Vec::new(),
        checksums: Vec::new(),
        shared: None,
    }
}

/// What a Win32 error code means for a file operation, where the OS text isn't clear enough.
pub(crate) fn win32_reason(code: i32) -> Option<(Option<ErrorKind>, &'static str)> {
    match code {
        // ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION
        32 | 33 => Some((None, "it's open in another app")),
        // ERROR_NOT_SAME_DEVICE
        17 => Some((Some(ErrorKind::Unsupported), "Windows can't move a folder to another drive in one step")),
        // ERROR_PRIVILEGE_NOT_HELD
        1314 => Some((
            Some(ErrorKind::PermissionDenied),
            "Windows needs Developer Mode or administrator rights to create symbolic links",
        )),
        _ => None,
    }
}

/// True when replacing a file failed because it's read-only or open in another app
/// (ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION).
pub(crate) fn replace_blocked(code: i32) -> bool {
    matches!(code, 5 | 32 | 33)
}
