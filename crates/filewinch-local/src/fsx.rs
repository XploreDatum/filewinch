//! Filesystem helpers shared by every OS: entries from metadata and human error messages.
//! The per-OS parts (owner names, exclusive rename, attributes) live in `unix.rs` and
//! `windows.rs`.

use std::fs;
use std::path::Path;

use filewinch_core::{Entry, EntryKind, Error, ErrorKind, RPath};

pub use crate::sys::{Names, rename_exclusive};

/// A human error for a failed filesystem call: "Couldn't open “notes.txt”: permission denied."
/// The full path and the OS error go in `detail`.
pub fn io_error(e: std::io::Error, verb: &str, path: &Path) -> Error {
    let code = e.raw_os_error();
    let mut base = Error::from(e);
    let mut why = None;
    if cfg!(windows)
        && let Some((kind, reason)) = code.and_then(crate::winpath::win32_reason)
    {
        base.kind = kind.unwrap_or(base.kind);
        why = Some(reason.to_owned());
    }
    let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
    let reason = why.unwrap_or_else(|| match base.kind {
        ErrorKind::NotFound => "it doesn't exist".to_owned(),
        ErrorKind::PermissionDenied => "you don't have permission".to_owned(),
        ErrorKind::AlreadyExists => "an item with that name already exists".to_owned(),
        ErrorKind::NoSpace => "the disk is full".to_owned(),
        ErrorKind::InvalidPath => "the path isn't valid here".to_owned(),
        _ => base.message.to_lowercase(),
    });
    Error { message: format!("Couldn't {verb} “{name}”: {reason}."), detail: Some(format!("{}: {}", path.display(), base.message)), ..base }
}

pub fn kind(md: &fs::Metadata) -> EntryKind {
    let ft = md.file_type();
    if ft.is_symlink() {
        EntryKind::Symlink
    } else if ft.is_dir() {
        EntryKind::Dir
    } else if ft.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

/// An [`Entry`] from `lstat` metadata.
pub fn entry(name: String, path: RPath, md: &fs::Metadata, local: &Path, names: &mut Names) -> Entry {
    let kind = kind(md);
    let link_target = if kind == EntryKind::Symlink {
        fs::read_link(local).ok().map(|t| {
            let t = t.to_string_lossy().into_owned();
            if cfg!(windows) { crate::winpath::link_target_from_windows(&t) } else { t }
        })
    } else {
        None
    };
    // A symlink reports its target's size, so a linked file shows what it holds.
    let size = match kind {
        EntryKind::File => Some(md.len()),
        EntryKind::Symlink => fs::metadata(local).ok().filter(|m| m.is_file()).map(|m| m.len()),
        _ => None,
    };
    let mut extra = Vec::new();
    if let Ok(created) = md.created() {
        extra.push(("Created".to_owned(), crate::fsx::rfc3339(created)));
    }
    #[cfg(windows)]
    if crate::sys::is_hidden(md) {
        extra.push((crate::HIDDEN_EXTRA.0.to_owned(), crate::HIDDEN_EXTRA.1.to_owned()));
    }
    let (mode, owner, group) = names.fields(md);
    Entry {
        name,
        path,
        kind,
        size,
        modified: md.modified().ok(),
        mode,
        owner,
        group,
        link_target,
        extra, checksums: Vec::new(), shared: None,
    }
}

/// A UTC timestamp, `2026-11-20T08:15:00Z`, for Get Info's extras.
pub fn rfc3339(t: std::time::SystemTime) -> String {
    let secs = t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}
