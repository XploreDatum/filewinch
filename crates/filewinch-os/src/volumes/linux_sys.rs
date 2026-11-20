//! Linux reader: `/proc/self/mounts`, then each volume's size from `statvfs`, given
//! [`SIZE_TIMEOUT`] so an NFS or SMB share that stopped answering can't hold the list up.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;

use super::linux::linux_volumes;
use super::{SIZE_TIMEOUT, Volume, bounded};

pub(super) fn read() -> Vec<Volume> {
    let Ok(text) = std::fs::read_to_string("/proc/self/mounts") else { return Vec::new() };
    let user = std::env::var("USER").unwrap_or_default();
    let mut v = linux_volumes(&text, &user);
    for vol in &mut v {
        let Ok(path) = CString::new(vol.path.as_os_str().as_bytes()) else { continue };
        if let Some(Some((total, free))) = bounded(SIZE_TIMEOUT, move || statvfs(&path)) {
            vol.total = Some(total);
            vol.free = Some(free);
        }
    }
    v
}

fn statvfs(path: &CString) -> Option<(u64, u64)> {
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: NUL-terminated path, and `s` is a valid out-pointer.
    if unsafe { libc::statvfs(path.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let unit = s.f_frsize as u64;
    Some(((s.f_blocks as u64).saturating_mul(unit), (s.f_bavail as u64).saturating_mul(unit)))
}
