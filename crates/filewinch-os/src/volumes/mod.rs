//! The disks and shares mounted on this computer, for the pane's location menu: the boot disk,
//! external drives, network shares and disk images, each with its free space.
//!
//! Every OS's rules are pure functions over plain data (a `statfs` entry, a line of
//! `/proc/self/mounts`, a Windows drive type), so they're tested on any OS. The thin readers
//! that gather that data block, sometimes for long (a network share that stopped answering):
//! call [`read`] off the UI thread.
//!
//! - macOS: `getmntinfo(MNT_NOWAIT)` (cached sizes, so a dead share can't block), plus
//!   DiskArbitration for whether a disk is internal, external or a disk image, and the boot
//!   volume's name ("Macintosh HD").
//! - Linux: `/proc/self/mounts`, then `statvfs` with a timeout per volume.
//! - Windows: `GetLogicalDrives`, `GetDriveTypeW`, then `GetVolumeInformationW` and
//!   `GetDiskFreeSpaceExW` with a timeout per drive.

pub mod linux;
pub mod mac;
pub mod windows;
#[cfg(test)]
mod tests;

#[cfg(target_os = "macos")]
mod mac_sys;
#[cfg(not(any(target_os = "macos", windows)))]
mod linux_sys;
#[cfg(windows)]
mod win_sys;

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

/// What kind of place a volume is, for its icon and the words beside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum VolumeKind {
    /// A disk inside the computer.
    Internal,
    /// A USB, Thunderbolt or SD-card drive.
    External,
    /// A mounted disk image (`.dmg`, `.iso`).
    DiskImage,
    /// A CD, DVD or Blu-ray drive.
    Optical,
    /// An SMB, AFP, NFS or WebDAV share.
    Network,
}

impl VolumeKind {
    /// "External disk", for the pane header and the menu.
    pub fn describe(self) -> &'static str {
        match self {
            VolumeKind::Internal => "Internal disk",
            VolumeKind::External => "External disk",
            VolumeKind::DiskImage => "Disk image",
            VolumeKind::Optical => "CD or DVD",
            VolumeKind::Network => "Network share",
        }
    }
}

/// One mounted volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    /// What people call it: "Macintosh HD", "CMG-512M2", "Backup (D:)".
    pub name: String,
    /// Where it's mounted: `/Volumes/CMG-512M2`, `C:\`.
    pub path: PathBuf,
    pub kind: VolumeKind,
    /// The disk the system runs from (macOS `/`, Linux `/`, Windows' system drive).
    pub boot: bool,
    /// Size and free space in bytes; `None` when the volume didn't say in time.
    pub total: Option<u64>,
    pub free: Option<u64>,
}

/// The volumes mounted now, boot disk first, then internal, external, disk images, optical
/// and network, each group by name. Blocks: call it off the UI thread.
pub fn read() -> Vec<Volume> {
    #[cfg(target_os = "macos")]
    let mut v = mac_sys::read();
    #[cfg(windows)]
    let mut v = win_sys::read();
    #[cfg(not(any(target_os = "macos", windows)))]
    let mut v = linux_sys::read();
    sort(&mut v);
    v
}

/// Boot disk first, then by kind, then by name as people read it.
pub fn sort(v: &mut [Volume]) {
    v.sort_by(|a, b| {
        b.boot.cmp(&a.boot).then(a.kind.cmp(&b.kind)).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// The volume `path` is on: the one with the deepest mount point that contains it (so
/// `/Volumes/A/B` is on A, not on `/`). Compares whole components, so `/Volumes/AB` isn't on A.
pub fn containing<'a>(volumes: &'a [Volume], path: &Path) -> Option<&'a Volume> {
    volumes.iter().filter(|v| path.starts_with(&v.path)).max_by_key(|v| v.path.components().count())
}

/// "340 GB free", or `None` when the size isn't known. Decimal units, as Finder and Explorer
/// count them.
pub fn free_text(v: &Volume) -> Option<String> {
    v.free.map(|b| format!("{} free", decimal_size(b)))
}

/// "340 GB", "1.2 TB", "512 MB", "0 bytes".
pub fn decimal_size(bytes: u64) -> String {
    if bytes < 1000 {
        return if bytes == 1 { "1 byte".into() } else { format!("{bytes} bytes") };
    }
    let units = ["KB", "MB", "GB", "TB", "PB"];
    let mut v = bytes as f64 / 1000.0;
    let mut u = 0;
    while v >= 999.5 && u + 1 < units.len() {
        v /= 1000.0;
        u += 1;
    }
    if v < 9.95 { format!("{v:.1} {}", units[u]) } else { format!("{v:.0} {}", units[u]) }
}

/// Runs `f` on its own thread and waits at most `limit` for it. A call that doesn't return in
/// time (a share whose server went away) is left to finish on its own; its answer is dropped.
pub fn bounded<T: Send + 'static>(limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("filewinch-volume".into())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .ok()?;
    rx.recv_timeout(limit).ok()
}

/// How long one volume may take to report its size before the menu shows it without one.
pub const SIZE_TIMEOUT: Duration = Duration::from_millis(1500);
