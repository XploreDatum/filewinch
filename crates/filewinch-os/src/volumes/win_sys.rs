//! Windows reader: `GetLogicalDrives` and `GetDriveTypeW` (instant), then each drive's label and
//! size with [`SIZE_TIMEOUT`], so a mapped drive whose server went away can't hold the list up.

use std::path::PathBuf;

use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW};

use super::windows::{drive_kind, drive_letters, drive_name};
use super::{SIZE_TIMEOUT, Volume, bounded};

pub(super) fn read() -> Vec<Volume> {
    // SAFETY: no arguments.
    let mask = unsafe { GetLogicalDrives() };
    let system = std::env::var("SystemDrive").ok().and_then(|d| d.chars().next()).unwrap_or('C').to_ascii_uppercase();
    drive_letters(mask)
        .into_iter()
        .filter_map(|letter| {
            let root = format!("{letter}:\\");
            let wide = wide(&root);
            // SAFETY: NUL-terminated root path.
            let kind = drive_kind(unsafe { GetDriveTypeW(wide.as_ptr()) })?;
            let (label, sizes) = bounded(SIZE_TIMEOUT, move || (label(&wide), sizes(&wide))).unwrap_or((None, None));
            Some(Volume {
                name: drive_name(label.as_deref().unwrap_or(""), letter, kind),
                path: PathBuf::from(root),
                kind,
                boot: letter == system,
                total: sizes.map(|s| s.0),
                free: sizes.map(|s| s.1),
            })
        })
        .collect()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn label(root: &[u16]) -> Option<String> {
    let mut name = [0u16; 261];
    // SAFETY: NUL-terminated root, and a buffer of the length passed; other outputs are null.
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            name.as_mut_ptr(),
            name.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        return None;
    }
    let n = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    Some(String::from_utf16_lossy(&name[..n]))
}

fn sizes(root: &[u16]) -> Option<(u64, u64)> {
    let (mut free, mut total) = (0u64, 0u64);
    // SAFETY: NUL-terminated root and valid out-pointers; the total-free output is unused.
    let ok = unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut free, &mut total, std::ptr::null_mut()) };
    (ok != 0).then_some((total, free))
}
