//! macOS and Linux: owner and group names, Unix mode bits, and a rename that never replaces
//! an existing item (`renamex_np(RENAME_EXCL)` on macOS, `renameat2(RENAME_NOREPLACE)` on
//! Linux).

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use filewinch_core::{Error, ErrorKind, Result};

use crate::fsx::io_error;

/// User and group names by id, looked up once per listing.
#[derive(Default)]
pub struct Names {
    users: HashMap<u32, Option<String>>,
    groups: HashMap<u32, Option<String>>,
}

impl Names {
    fn user(&mut self, uid: u32) -> Option<String> {
        self.users.entry(uid).or_insert_with(|| lookup_user(uid)).clone()
    }
    fn group(&mut self, gid: u32) -> Option<String> {
        self.groups.entry(gid).or_insert_with(|| lookup_group(gid)).clone()
    }

    /// An entry's mode bits, owner and group.
    pub fn fields(&mut self, md: &fs::Metadata) -> (Option<u32>, Option<String>, Option<String>) {
        (Some(md.permissions().mode() & 0o7777), self.user(md.uid()), self.group(md.gid()))
    }
}

fn lookup_user(uid: u32) -> Option<String> {
    let mut buf = vec![0 as libc::c_char; 4096];
    // SAFETY: zeroed passwd is a valid out-parameter; buf outlives the call and the result
    // pointers into it are read before buf drops.
    unsafe {
        let mut pw: libc::passwd = std::mem::zeroed();
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        if libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr(), buf.len(), &mut out) != 0 || out.is_null() {
            return None;
        }
        Some(CStr::from_ptr(pw.pw_name).to_string_lossy().into_owned())
    }
}

fn lookup_group(gid: u32) -> Option<String> {
    let mut buf = vec![0 as libc::c_char; 4096];
    // SAFETY: as in `lookup_user`.
    unsafe {
        let mut gr: libc::group = std::mem::zeroed();
        let mut out: *mut libc::group = std::ptr::null_mut();
        if libc::getgrgid_r(gid, &mut gr, buf.as_mut_ptr(), buf.len(), &mut out) != 0 || out.is_null() {
            return None;
        }
        Some(CStr::from_ptr(gr.gr_name).to_string_lossy().into_owned())
    }
}

/// Sets the Unix mode bits.
pub fn set_mode(p: &Path, mode: u32) -> std::io::Result<()> {
    fs::set_permissions(p, fs::Permissions::from_mode(mode & 0o7777))
}

/// Renames without ever replacing what's at `to`.
pub fn rename_exclusive(from: &Path, to: &Path) -> Result<()> {
    let c = |p: &Path| {
        CString::new(p.as_os_str().as_bytes())
            .map_err(|_| Error::new(ErrorKind::InvalidPath, format!("“{}” contains a NUL character.", p.display())))
    };
    let (cf, ct) = (c(from)?, c(to)?);
    let Err(err) = exclusive(&cf, &ct) else { return Ok(()) };
    if err.raw_os_error() == Some(libc::EEXIST) {
        let name = to.file_name().map_or_else(|| to.display().to_string(), |n| n.to_string_lossy().into_owned());
        return Err(Error::new(ErrorKind::AlreadyExists, format!("Couldn't rename: “{name}” already exists.")));
    }
    // Volumes without exclusive rename (some network shares, older Linux kernels and
    // filesystems): check, then rename.
    if matches!(err.raw_os_error(), Some(libc::ENOTSUP) | Some(libc::EINVAL) | Some(libc::ENOSYS)) {
        if fs::symlink_metadata(to).is_ok() {
            return Err(Error::new(ErrorKind::AlreadyExists, format!("Couldn't rename: “{}” already exists.", to.display())));
        }
        return fs::rename(from, to).map_err(|e| io_error(e, "rename", from));
    }
    Err(io_error(err, "rename", from))
}

#[cfg(target_os = "macos")]
fn exclusive(from: &CStr, to: &CStr) -> std::io::Result<()> {
    // SAFETY: both are valid NUL-terminated strings that outlive the call.
    let rc = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

#[cfg(target_os = "linux")]
fn exclusive(from: &CStr, to: &CStr) -> std::io::Result<()> {
    // SAFETY: both are valid NUL-terminated strings that outlive the call; AT_FDCWD makes
    // relative paths resolve as `rename` would.
    let rc = unsafe { libc::renameat2(libc::AT_FDCWD, from.as_ptr(), libc::AT_FDCWD, to.as_ptr(), libc::RENAME_NOREPLACE) };
    if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

/// Other Unix systems have no exclusive rename call: take the check-then-rename path.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn exclusive(_from: &CStr, _to: &CStr) -> std::io::Result<()> {
    Err(std::io::Error::from_raw_os_error(libc::ENOTSUP))
}
