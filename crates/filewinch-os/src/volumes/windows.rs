//! Windows: what a drive type means and what a drive is called. Pure, so it's tested on any OS;
//! `windows_sys` asks Windows.

use super::VolumeKind;

/// `GetDriveTypeW` results (`fileapi.h`).
pub const DRIVE_UNKNOWN: u32 = 0;
pub const DRIVE_NO_ROOT_DIR: u32 = 1;
pub const DRIVE_REMOVABLE: u32 = 2;
pub const DRIVE_FIXED: u32 = 3;
pub const DRIVE_REMOTE: u32 = 4;
pub const DRIVE_CDROM: u32 = 5;
pub const DRIVE_RAMDISK: u32 = 6;

/// The kind a drive type is, or `None` for a letter with nothing behind it.
pub fn drive_kind(drive_type: u32) -> Option<VolumeKind> {
    match drive_type {
        DRIVE_FIXED | DRIVE_RAMDISK => Some(VolumeKind::Internal),
        DRIVE_REMOVABLE => Some(VolumeKind::External),
        DRIVE_REMOTE => Some(VolumeKind::Network),
        DRIVE_CDROM => Some(VolumeKind::Optical),
        _ => None,
    }
}

/// "Backup (D:)", as File Explorer names a drive; without a label, Explorer's own words for
/// its kind ("Local Disk (C:)", "USB Drive (E:)").
pub fn drive_name(label: &str, letter: char, kind: VolumeKind) -> String {
    let letter = letter.to_ascii_uppercase();
    let label = label.trim();
    let label = if label.is_empty() {
        match kind {
            VolumeKind::Internal => "Local Disk",
            VolumeKind::External => "USB Drive",
            VolumeKind::Network => "Network Drive",
            VolumeKind::Optical => "CD Drive",
            VolumeKind::DiskImage => "Disk Image",
        }
    } else {
        label
    };
    format!("{label} ({letter}:)")
}

/// The letters set in a `GetLogicalDrives` mask: bit 0 is A.
pub fn drive_letters(mask: u32) -> Vec<char> {
    (0..26u8).filter(|i| mask & (1 << i) != 0).map(|i| (b'A' + i) as char).collect()
}
