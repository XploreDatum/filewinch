//! macOS: which `statfs` entries are volumes a person browses, and what kind each is. Pure, so
//! it's tested on any OS; `mac_sys` gathers the entries.

use std::path::PathBuf;

use super::{Volume, VolumeKind};

/// `statfs.f_flags` bits (`<sys/mount.h>`), spelled out so the rules build on every OS.
pub const MNT_RDONLY: u32 = 0x0000_0001;
pub const MNT_LOCAL: u32 = 0x0000_1000;
pub const MNT_ROOTFS: u32 = 0x0000_4000;
pub const MNT_DONTBROWSE: u32 = 0x0010_0000;
pub const MNT_AUTOMOUNTED: u32 = 0x0040_0000;

/// One `getmntinfo` entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MacMount {
    /// `f_mntonname`: `/Volumes/CMG-512M2`.
    pub path: String,
    /// `f_mntfromname`: `/dev/disk16s3`, `//me@nas/share`.
    pub from: String,
    /// `f_fstypename`: `apfs`, `exfat`, `smbfs`.
    pub fstype: String,
    pub flags: u32,
    /// `f_blocks * f_bsize` and `f_bavail * f_bsize`.
    pub total: u64,
    pub free: u64,
}

/// What DiskArbitration says about the disk behind a mount, when it answers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiskInfo {
    /// The volume's own name ("Macintosh HD"), which `/`'s mount path doesn't carry.
    pub name: Option<String>,
    /// `DeviceInternal`: on the computer's own bus.
    pub internal: Option<bool>,
    /// `MediaEjectable` or `MediaRemovable`.
    pub ejectable: bool,
    /// `DeviceModel` ("Disk Image") and `DeviceProtocol` ("USB", "Virtual Interface").
    pub model: Option<String>,
    pub protocol: Option<String>,
}

/// File systems that are plumbing, not places: never listed.
const PSEUDO: &[&str] = &["devfs", "autofs", "nullfs", "devicefs", "lifs", "mtmfs", "fdesc"];
/// File systems that reach a server.
const NETWORK: &[&str] = &["smbfs", "afpfs", "nfs", "webdav", "cifs", "ftp"];
/// System volumes macOS mounts under `/Volumes` on some versions; hidden as Finder hides them.
const SYSTEM: &[&str] = &["Recovery", "Preboot", "VM", "Update"];

/// The volume a mount is, or `None` when Finder wouldn't show it: the system's own volumes
/// (`/System/Volumes/*`, Recovery, VM), `nobrowse` mounts, pseudo file systems, hidden names,
/// and anything outside `/` and `/Volumes`.
pub fn mac_volume(m: &MacMount, disk: Option<&DiskInfo>) -> Option<Volume> {
    if PSEUDO.contains(&m.fstype.as_str()) || m.flags & MNT_DONTBROWSE != 0 {
        return None;
    }
    let network = NETWORK.contains(&m.fstype.as_str()) || m.flags & MNT_LOCAL == 0;
    let size = |n: u64| (n > 0 || !network).then_some(n);
    if m.path == "/" {
        let name = disk.and_then(|d| d.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| "Macintosh HD".into());
        return Some(Volume { name, path: PathBuf::from("/"), kind: VolumeKind::Internal, boot: true, total: size(m.total), free: size(m.free) });
    }
    let name = m.path.strip_prefix("/Volumes/")?;
    if name.is_empty() || name.contains('/') || name.starts_with('.') || SYSTEM.contains(&name) {
        return None;
    }
    let kind = if network {
        VolumeKind::Network
    } else {
        local_kind(&m.fstype, disk)
    };
    Some(Volume { name: name.to_owned(), path: PathBuf::from(&m.path), kind, boot: false, total: size(m.total), free: size(m.free) })
}

/// A local, non-boot volume's kind. Without DiskArbitration's answer, a volume under `/Volumes`
/// is taken as external (an internal second volume is the rarer case) and `cd9660`/`udf` as a disc.
fn local_kind(fstype: &str, disk: Option<&DiskInfo>) -> VolumeKind {
    let optical = matches!(fstype, "cd9660" | "udf" | "cddafs");
    let Some(d) = disk else {
        return if optical { VolumeKind::Optical } else { VolumeKind::External };
    };
    let image = d.model.as_deref().is_some_and(|m| m.eq_ignore_ascii_case("Disk Image"))
        || d.protocol.as_deref().is_some_and(|p| p.eq_ignore_ascii_case("Virtual Interface"));
    if image {
        VolumeKind::DiskImage
    } else if optical {
        VolumeKind::Optical
    } else if d.internal == Some(true) && !d.ejectable {
        VolumeKind::Internal
    } else {
        VolumeKind::External
    }
}

/// The BSD name DiskArbitration knows a mount's device by: `/dev/disk16s3` → `disk16s3`.
pub fn bsd_name(from: &str) -> Option<&str> {
    from.strip_prefix("/dev/").filter(|n| n.starts_with("disk"))
}
