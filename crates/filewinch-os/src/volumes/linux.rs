//! Linux: which `/proc/self/mounts` lines are places a person browses. Pure, so it's tested on
//! any OS; `linux_sys` reads the file and the sizes.

use std::collections::HashSet;
use std::path::PathBuf;

use super::{Volume, VolumeKind};

/// One `/proc/self/mounts` line: `device mount-point fstype options 0 0`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinuxMount {
    pub device: String,
    pub path: String,
    pub fstype: String,
}

/// File systems that reach a server.
const NETWORK: &[&str] = &["nfs", "nfs4", "cifs", "smb3", "smbfs", "fuse.sshfs", "sshfs", "davfs", "fuse.rclone", "9p", "afs", "ceph", "glusterfs"];
/// Real file systems whose source isn't a `/dev` node.
const POOLS: &[&str] = &["zfs", "bcachefs"];
/// Pseudo and container file systems, never places (also caught by the `/dev` rule; listed so
/// a `/dev/loop` snap or a tmpfs never slips through).
const PSEUDO: &[&str] = &[
    "proc", "sysfs", "tmpfs", "devtmpfs", "devpts", "overlay", "squashfs", "cgroup", "cgroup2", "securityfs", "pstore",
    "debugfs", "tracefs", "configfs", "fusectl", "mqueue", "hugetlbfs", "autofs", "binfmt_misc", "bpf", "efivarfs",
    "ramfs", "nsfs", "rpc_pipefs", "selinuxfs", "fuse.gvfsd-fuse", "fuse.portal", "fuse.snapfuse", "fuse.lxcfs",
];

/// Undoes the kernel's octal escapes: `\040` space, `\011` tab, `\012` newline, `\134` backslash.
pub fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && (b'0'..=b'3').contains(&b[i + 1]) && b[i + 2..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Every well-formed line of `/proc/self/mounts`, in order.
pub fn parse_mounts(text: &str) -> Vec<LinuxMount> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split_ascii_whitespace();
            let (device, path, fstype) = (f.next()?, f.next()?, f.next()?);
            Some(LinuxMount { device: unescape(device), path: unescape(path), fstype: fstype.to_owned() })
        })
        .collect()
}

/// True when `path` is `base` or inside it, by whole components.
fn under(path: &str, base: &str) -> bool {
    path == base || path.strip_prefix(base).is_some_and(|r| r.starts_with('/'))
}

/// The volume a mount is, or `None`: pseudo file systems, loop-mounted snaps, and mounts
/// outside `/`, `/home`, `/mnt`, `/media/<user>` and `/run/media/<user>` (no `/boot/efi`,
/// `/snap/*`, `/var/lib/docker/*`).
pub fn linux_volume(m: &LinuxMount, user: &str) -> Option<Volume> {
    if PSEUDO.contains(&m.fstype.as_str()) {
        return None;
    }
    let network = NETWORK.contains(&m.fstype.as_str());
    let block = m.device.starts_with("/dev/") && !m.device.starts_with("/dev/loop") || POOLS.contains(&m.fstype.as_str());
    let image = m.device.starts_with("/dev/loop");
    let media = [format!("/media/{user}"), format!("/run/media/{user}")];
    let in_media = !user.is_empty() && media.iter().any(|b| m.path.starts_with(&format!("{b}/")));
    // A loop device is a disk image only where the desktop mounted one the person opened.
    if !(network || block || image && in_media) {
        return None;
    }
    let boot = m.path == "/";
    let place = boot || under(&m.path, "/home") || under(&m.path, "/mnt") || in_media;
    if !place {
        return None;
    }
    let kind = if network {
        VolumeKind::Network
    } else if image {
        VolumeKind::DiskImage
    } else if matches!(m.fstype.as_str(), "iso9660" | "udf") {
        VolumeKind::Optical
    } else if in_media {
        VolumeKind::External
    } else {
        VolumeKind::Internal
    };
    let name = if boot {
        "File System".to_owned()
    } else {
        m.path.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or(&m.path).to_owned()
    };
    Some(Volume { name, path: PathBuf::from(&m.path), kind, boot, total: None, free: None })
}

/// The volumes in a `/proc/self/mounts` text, without sizes. A device mounted twice (a bind
/// mount, a btrfs subvolume) shows once, at its first mount point; a mount point mounted over
/// shows its newest mount.
pub fn linux_volumes(text: &str, user: &str) -> Vec<Volume> {
    let mounts = parse_mounts(text);
    // The last mount at a path is the one in effect.
    let mut live: Vec<&LinuxMount> = Vec::new();
    for m in &mounts {
        live.retain(|x| x.path != m.path);
        live.push(m);
    }
    let mut seen = HashSet::new();
    live.into_iter()
        .filter_map(|m| linux_volume(m, user).map(|v| (m, v)))
        .filter(|(m, _)| seen.insert(m.device.clone()))
        .map(|(_, v)| v)
        .collect()
}
