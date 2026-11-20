use std::path::{Path, PathBuf};

use super::linux::{linux_volumes, parse_mounts, unescape};
use super::mac::{DiskInfo, MNT_AUTOMOUNTED, MNT_DONTBROWSE, MNT_LOCAL, MNT_RDONLY, MNT_ROOTFS, MacMount, bsd_name, mac_volume};
use super::windows::{DRIVE_CDROM, DRIVE_FIXED, DRIVE_NO_ROOT_DIR, DRIVE_RAMDISK, DRIVE_REMOTE, DRIVE_REMOVABLE, DRIVE_UNKNOWN, drive_kind, drive_letters, drive_name};
use super::{Volume, VolumeKind, containing, decimal_size, free_text, sort};

fn mnt(path: &str, from: &str, fstype: &str, flags: u32) -> MacMount {
    MacMount { path: path.into(), from: from.into(), fstype: fstype.into(), flags, total: 500_000_000_000, free: 340_000_000_000 }
}

fn vol(name: &str, path: &str, kind: VolumeKind, boot: bool) -> Volume {
    Volume { name: name.into(), path: PathBuf::from(path), kind, boot, total: None, free: None }
}

// --- macOS ---

#[test]
fn mac_boot_volume_takes_its_name_not_slash() {
    let root = mnt("/", "/dev/disk3s1s1", "apfs", MNT_LOCAL | MNT_ROOTFS | MNT_RDONLY);
    let disk = DiskInfo { name: Some("Macintosh HD".into()), internal: Some(true), ..DiskInfo::default() };
    let v = mac_volume(&root, Some(&disk)).unwrap();
    assert_eq!((v.name.as_str(), v.kind, v.boot), ("Macintosh HD", VolumeKind::Internal, true));
    assert_eq!(v.free, Some(340_000_000_000));
    // DiskArbitration didn't answer: still a name, never "/".
    assert_eq!(mac_volume(&root, None).unwrap().name, "Macintosh HD");
    let renamed = DiskInfo { name: Some("Studio SSD".into()), ..DiskInfo::default() };
    assert_eq!(mac_volume(&root, Some(&renamed)).unwrap().name, "Studio SSD");
}

#[test]
fn mac_skips_system_hidden_and_pseudo_mounts() {
    let skipped = [
        mnt("/dev", "devfs", "devfs", MNT_LOCAL | MNT_DONTBROWSE),
        mnt("/System/Volumes/Data", "/dev/disk3s5", "apfs", MNT_LOCAL | MNT_DONTBROWSE),
        mnt("/System/Volumes/VM", "/dev/disk3s6", "apfs", MNT_LOCAL | MNT_DONTBROWSE),
        mnt("/System/Volumes/Preboot", "/dev/disk3s2", "apfs", MNT_LOCAL | MNT_DONTBROWSE),
        mnt("/System/Volumes/Update", "/dev/disk3s4", "apfs", MNT_LOCAL | MNT_DONTBROWSE),
        // Recovery without the nobrowse flag (older systems) is still hidden by name.
        mnt("/Volumes/Recovery", "/dev/disk3s3", "apfs", MNT_LOCAL),
        mnt("/System/Volumes/Data/home", "map auto_home", "autofs", MNT_AUTOMOUNTED | MNT_DONTBROWSE),
        mnt("/private/var/folders/x/T/AppTranslocation/ABC", "/Applications/X.app", "nullfs", MNT_LOCAL | MNT_RDONLY),
        mnt("/Users/me/Library/Developer/CoreDevice/DeviceFS", "devices", "devicefs", MNT_LOCAL),
        mnt("/Volumes/.timemachine", "/dev/disk9s1", "apfs", MNT_LOCAL),
        mnt("/Library/Developer/CoreSimulator/Volumes/iOS_22G86", "/dev/disk11s1", "apfs", MNT_LOCAL),
        mnt("/Volumes/Nested/Inner", "/dev/disk12s1", "apfs", MNT_LOCAL),
    ];
    for m in &skipped {
        assert_eq!(mac_volume(m, None), None, "{} is hidden", m.path);
    }
}

#[test]
fn mac_classifies_external_network_and_images() {
    let usb = DiskInfo { internal: Some(false), ejectable: true, protocol: Some("USB".into()), ..DiskInfo::default() };
    let v = mac_volume(&mnt("/Volumes/CMG-512M2", "/dev/disk16s3", "exfat", MNT_LOCAL), Some(&usb)).unwrap();
    assert_eq!((v.name.as_str(), v.kind, v.boot), ("CMG-512M2", VolumeKind::External, false));

    let image = DiskInfo { internal: Some(true), model: Some("Disk Image".into()), protocol: Some("Virtual Interface".into()), ..DiskInfo::default() };
    assert_eq!(mac_volume(&mnt("/Volumes/Installer", "/dev/disk20s1", "hfs", MNT_LOCAL | MNT_RDONLY), Some(&image)).unwrap().kind, VolumeKind::DiskImage);

    // A second volume on the internal disk stays internal.
    let inside = DiskInfo { internal: Some(true), protocol: Some("Apple Fabric".into()), ..DiskInfo::default() };
    assert_eq!(mac_volume(&mnt("/Volumes/Projects", "/dev/disk3s7", "apfs", MNT_LOCAL), Some(&inside)).unwrap().kind, VolumeKind::Internal);
    // An SD card on the internal reader is ejectable: external.
    let sd = DiskInfo { internal: Some(true), ejectable: true, ..DiskInfo::default() };
    assert_eq!(mac_volume(&mnt("/Volumes/CAMERA", "/dev/disk5s1", "msdos", MNT_LOCAL), Some(&sd)).unwrap().kind, VolumeKind::External);

    for fs in ["smbfs", "afpfs", "nfs", "webdav"] {
        let mut m = mnt("/Volumes/share", "//me@nas.local/share", fs, 0);
        m.total = 0;
        m.free = 0;
        let v = mac_volume(&m, None).unwrap();
        assert_eq!(v.kind, VolumeKind::Network, "{fs}");
        // A share that reported nothing has no size rather than "Zero bytes free".
        assert_eq!((v.total, v.free), (None, None));
    }
    // No DiskArbitration answer: by flags and type.
    assert_eq!(mac_volume(&mnt("/Volumes/USB", "/dev/disk6s1", "msdos", MNT_LOCAL), None).unwrap().kind, VolumeKind::External);
    assert_eq!(mac_volume(&mnt("/Volumes/DVD", "/dev/disk7", "cd9660", MNT_LOCAL | MNT_RDONLY), None).unwrap().kind, VolumeKind::Optical);
    assert_eq!(mac_volume(&mnt("/Volumes/odd", "server:/x", "apfs", 0), None).unwrap().kind, VolumeKind::Network);
}

#[test]
fn mac_bsd_names() {
    assert_eq!(bsd_name("/dev/disk16s3"), Some("disk16s3"));
    assert_eq!(bsd_name("//me@nas/share"), None);
    assert_eq!(bsd_name("map auto_home"), None);
}

// --- Linux ---

const MOUNTS: &str = "\
sysfs /sys sysfs rw,nosuid,nodev,noexec,relatime 0 0
proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0
udev /dev devtmpfs rw,nosuid,relatime 0 0
tmpfs /run tmpfs rw,nosuid,nodev,noexec 0 0
/dev/nvme0n1p2 / ext4 rw,relatime 0 0
cgroup2 /sys/fs/cgroup cgroup2 rw 0 0
/dev/nvme0n1p1 /boot/efi vfat rw 0 0
/dev/loop3 /snap/core22/1380 squashfs ro,nodev 0 0
/dev/loop4 /snap/firefox/4848 squashfs ro,nodev 0 0
/dev/nvme0n1p3 /home ext4 rw 0 0
overlay /var/lib/docker/overlay2/abc/merged overlay rw 0 0
tmpfs /run/user/1000 tmpfs rw 0 0
/dev/sdb1 /media/alex/My\\040Passport ext4 rw,nosuid,nodev 0 0
/dev/sdc1 /run/media/alex/CAMERA vfat rw 0 0
/dev/sr0 /media/alex/Ubuntu\\04024.04 iso9660 ro 0 0
/dev/loop9 /media/alex/Installer iso9660 ro 0 0
/dev/nvme0n1p3 /mnt/home-bind ext4 rw 0 0
nas.local:/export/media /mnt/media nfs4 rw 0 0
//nas.local/share /mnt/share cifs rw 0 0
gvfsd-fuse /run/user/1000/gvfs fuse.gvfsd-fuse rw 0 0
/dev/sdd1 /media/bob/Theirs ext4 rw 0 0
/dev/sde1 /srv/data ext4 rw 0 0
";

#[test]
fn linux_unescapes_mount_paths() {
    assert_eq!(unescape("/media/alex/My\\040Passport"), "/media/alex/My Passport");
    assert_eq!(unescape("a\\011b\\012c\\134d"), "a\tb\nc\\d");
    // Not an escape: left alone, never a panic.
    assert_eq!(unescape("x\\9y\\"), "x\\9y\\");
    assert_eq!(unescape("\\777"), "\\777");
    let m = parse_mounts("/dev/sdb1 /media/alex/My\\040Passport ext4 rw 0 0\nbroken\n");
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].path, "/media/alex/My Passport");
}

#[test]
fn linux_lists_real_disks_and_shares_only() {
    let v = linux_volumes(MOUNTS, "alex");
    let got: Vec<(&str, &Path, VolumeKind, bool)> = v.iter().map(|v| (v.name.as_str(), v.path.as_path(), v.kind, v.boot)).collect();
    assert_eq!(
        got,
        vec![
            ("File System", Path::new("/"), VolumeKind::Internal, true),
            ("home", Path::new("/home"), VolumeKind::Internal, false),
            ("My Passport", Path::new("/media/alex/My Passport"), VolumeKind::External, false),
            ("CAMERA", Path::new("/run/media/alex/CAMERA"), VolumeKind::External, false),
            ("Ubuntu 24.04", Path::new("/media/alex/Ubuntu 24.04"), VolumeKind::Optical, false),
            ("Installer", Path::new("/media/alex/Installer"), VolumeKind::DiskImage, false),
            // `/mnt/home-bind` is a bind mount of /home's device: shown once.
            ("media", Path::new("/mnt/media"), VolumeKind::Network, false),
            ("share", Path::new("/mnt/share"), VolumeKind::Network, false),
        ]
    );
}

#[test]
fn linux_newest_mount_at_a_path_wins() {
    let text = "/dev/sdb1 /mnt/x ext4 rw 0 0\nnas:/a /mnt/x nfs rw 0 0\n";
    let v = linux_volumes(text, "me");
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].kind, VolumeKind::Network);
    // No user: nothing under /media counts.
    assert!(linux_volumes("/dev/sdb1 /media//x ext4 rw 0 0\n", "").is_empty());
}

// --- Windows ---

#[test]
fn windows_drive_types() {
    assert_eq!(drive_kind(DRIVE_FIXED), Some(VolumeKind::Internal));
    assert_eq!(drive_kind(DRIVE_RAMDISK), Some(VolumeKind::Internal));
    assert_eq!(drive_kind(DRIVE_REMOVABLE), Some(VolumeKind::External));
    assert_eq!(drive_kind(DRIVE_REMOTE), Some(VolumeKind::Network));
    assert_eq!(drive_kind(DRIVE_CDROM), Some(VolumeKind::Optical));
    assert_eq!(drive_kind(DRIVE_UNKNOWN), None);
    assert_eq!(drive_kind(DRIVE_NO_ROOT_DIR), None);
}

#[test]
fn windows_drive_names() {
    assert_eq!(drive_name("Backup", 'd', VolumeKind::Internal), "Backup (D:)");
    assert_eq!(drive_name("  ", 'C', VolumeKind::Internal), "Local Disk (C:)");
    assert_eq!(drive_name("", 'E', VolumeKind::External), "USB Drive (E:)");
    assert_eq!(drive_name("", 'Z', VolumeKind::Network), "Network Drive (Z:)");
    assert_eq!(drive_name("", 'F', VolumeKind::Optical), "CD Drive (F:)");
    assert_eq!(drive_letters((1 << 25) | 0b1101), vec!['A', 'C', 'D', 'Z']);
    assert!(drive_letters(0).is_empty());
}

// --- Shared ---

#[test]
fn free_space_reads_as_finder_says_it() {
    assert_eq!(decimal_size(0), "0 bytes");
    assert_eq!(decimal_size(1), "1 byte");
    assert_eq!(decimal_size(999), "999 bytes");
    assert_eq!(decimal_size(1_500), "1.5 KB");
    assert_eq!(decimal_size(340_123_456_789), "340 GB");
    assert_eq!(decimal_size(9_960_000_000), "10 GB");
    assert_eq!(decimal_size(999_600_000_000), "1.0 TB");
    assert_eq!(decimal_size(1_234_000_000_000), "1.2 TB");
    let mut v = vol("A", "/Volumes/A", VolumeKind::External, false);
    assert_eq!(free_text(&v), None);
    v.free = Some(120_000_000_000);
    assert_eq!(free_text(&v).as_deref(), Some("120 GB free"));
}

#[test]
fn deepest_mount_wins() {
    let vols = [
        vol("Macintosh HD", "/", VolumeKind::Internal, true),
        vol("A", "/Volumes/A", VolumeKind::External, false),
        vol("Inner", "/Volumes/A/B/mnt", VolumeKind::DiskImage, false),
    ];
    assert_eq!(containing(&vols, Path::new("/Volumes/A/B")).unwrap().name, "A");
    assert_eq!(containing(&vols, Path::new("/Volumes/A")).unwrap().name, "A");
    assert_eq!(containing(&vols, Path::new("/Volumes/A/B/mnt/x")).unwrap().name, "Inner");
    // Whole components only: AB isn't on A.
    assert_eq!(containing(&vols, Path::new("/Volumes/AB")).unwrap().name, "Macintosh HD");
    assert_eq!(containing(&vols, Path::new("/Users/me")).unwrap().name, "Macintosh HD");
    assert!(containing(&vols[1..], Path::new("/Users/me")).is_none());
}

#[test]
fn boot_first_then_kind_then_name() {
    let mut v = vec![
        vol("share", "/Volumes/share", VolumeKind::Network, false),
        vol("zeta", "/Volumes/zeta", VolumeKind::External, false),
        vol("Alpha", "/Volumes/Alpha", VolumeKind::External, false),
        vol("Installer", "/Volumes/Installer", VolumeKind::DiskImage, false),
        vol("Macintosh HD", "/", VolumeKind::Internal, true),
    ];
    sort(&mut v);
    let names: Vec<&str> = v.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(names, ["Macintosh HD", "Alpha", "zeta", "Installer", "share"]);
}

#[test]
fn a_slow_call_gives_up_in_time() {
    use std::time::{Duration, Instant};
    let start = Instant::now();
    let got = super::bounded(Duration::from_millis(50), || std::thread::sleep(Duration::from_secs(2)));
    assert!(got.is_none());
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(super::bounded(Duration::from_secs(5), || 7), Some(7));
}

/// The real reader on this machine: run with `--ignored --nocapture` to see what it finds.
#[test]
#[ignore]
fn print_this_computers_volumes() {
    for v in super::read() {
        println!("{:<24} {:<10?} boot={} free={:?} total={:?} {}", v.name, v.kind, v.boot, free_text(&v), v.total.map(decimal_size), v.path.display());
    }
}
