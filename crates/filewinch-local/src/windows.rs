//! Windows: a rename that never replaces an existing item (`MoveFileExW` without
//! `MOVEFILE_REPLACE_EXISTING`), the drive list, and the hidden and read-only attributes.
//! Windows has no Unix mode bits, owners or groups, so entries carry none.

use std::fs;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

use filewinch_core::{Entry, Error, ErrorKind, Result};
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_WRITE_ATTRIBUTES, GetFileAttributesW, GetLogicalDrives, INVALID_FILE_ATTRIBUTES, MOVEFILE_COPY_ALLOWED,
    MoveFileExW, SetFileAttributesW,
};

use crate::fsx::io_error;

/// Windows entries have no owner or group names to look up.
#[derive(Default)]
pub struct Names {}

impl Names {
    /// Windows has no Unix mode bits, owner or group.
    pub fn fields(&mut self, _md: &fs::Metadata) -> (Option<u32>, Option<String>, Option<String>) {
        (None, None, None)
    }
}

/// `p` as a NUL-terminated wide string, in verbatim form (`\\?\C:\…`, `\\?\UNC\server\…`)
/// so paths over 260 characters work. Only used for paths built by `winpath::windows_path`,
/// which are absolute and hold no `.`, `..` or `/`.
fn wide(p: &Path) -> Vec<u16> {
    let w: Vec<u16> = p.as_os_str().encode_wide().collect();
    let starts = |s: &str| w.len() >= s.len() && s.encode_utf16().zip(&w).all(|(a, &b)| a == b);
    let mut out: Vec<u16> = Vec::with_capacity(w.len() + 8);
    let body: &[u16] = if starts(r"\\?\") || starts(r"\\.\") {
        &w
    } else if starts(r"\\") {
        out.extend(r"\\?\UNC".encode_utf16());
        &w[1..]
    } else if w.len() >= 3 && w[1] == u16::from(b':') && w[2] == u16::from(b'\\') {
        out.extend(r"\\?\".encode_utf16());
        &w
    } else {
        &w
    };
    out.extend_from_slice(body);
    out.push(0);
    out
}

/// Renames without ever replacing what's at `to`; a file may move to another drive.
pub fn rename_exclusive(from: &Path, to: &Path) -> Result<()> {
    let (f, t) = (wide(from), wide(to));
    // SAFETY: both are NUL-terminated and outlive the call.
    if unsafe { MoveFileExW(f.as_ptr(), t.as_ptr(), MOVEFILE_COPY_ALLOWED) } != 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if matches!(err.raw_os_error().map(|c| c as u32), Some(ERROR_ALREADY_EXISTS | ERROR_FILE_EXISTS)) {
        let name = to.file_name().map_or_else(|| to.display().to_string(), |n| n.to_string_lossy().into_owned());
        return Err(Error::new(ErrorKind::AlreadyExists, format!("Couldn't rename: “{name}” already exists.")).with_detail(err));
    }
    Err(io_error(err, "rename", from))
}

/// This computer's drives, for the root listing.
pub fn drive_entries() -> Result<Vec<Entry>> {
    // SAFETY: takes no arguments; returns a bit mask, 0 on failure.
    let mask = unsafe { GetLogicalDrives() };
    if mask == 0 {
        let err = io::Error::last_os_error();
        return Err(Error::new(ErrorKind::Other, "Couldn't list this computer's drives.").with_detail(err));
    }
    Ok(crate::winpath::drive_entries(mask))
}

/// True when the item has the hidden attribute.
pub fn is_hidden(md: &fs::Metadata) -> bool {
    md.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0
}

/// True for a symbolic link or junction to a folder, which Windows deletes as a folder.
pub fn is_dir_link(md: &fs::Metadata) -> bool {
    md.file_type().is_symlink_dir()
}

/// Turns the hidden attribute on or off; best effort (a failure only shows the item).
pub fn set_hidden(p: &Path, on: bool) {
    set_attribute(p, FILE_ATTRIBUTE_HIDDEN, on);
}

/// Clears the read-only attribute so a leftover file can be deleted; best effort.
pub fn clear_readonly(p: &Path) {
    set_attribute(p, FILE_ATTRIBUTE_READONLY, false);
}

fn set_attribute(p: &Path, bit: u32, on: bool) {
    let w = wide(p);
    // SAFETY: `w` is NUL-terminated and outlives both calls.
    unsafe {
        let attrs = GetFileAttributesW(w.as_ptr());
        if attrs == INVALID_FILE_ATTRIBUTES {
            return;
        }
        // FILE_ATTRIBUTE_NORMAL is only valid alone.
        let mut next = if on { (attrs & !FILE_ATTRIBUTE_NORMAL) | bit } else { attrs & !bit };
        if next == 0 {
            next = FILE_ATTRIBUTE_NORMAL;
        }
        if next != attrs {
            SetFileAttributesW(w.as_ptr(), next);
        }
    }
}

/// Opens a file or folder for changing its dates. `File::open` is read-only on Windows and
/// `set_modified` needs FILE_WRITE_ATTRIBUTES (allowed on read-only files too); folders
/// open only with FILE_FLAG_BACKUP_SEMANTICS.
pub fn open_for_dates(p: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new().access_mode(FILE_WRITE_ATTRIBUTES).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(p)
}
