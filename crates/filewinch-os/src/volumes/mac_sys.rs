//! macOS reader: `getfsstat(MNT_NOWAIT)` (the thread-safe `getmntinfo`; cached sizes, so a
//! share that stopped answering can't block) and DiskArbitration for each local disk.

use std::ffi::{CStr, c_char, c_void};
use std::path::Path;

use core_foundation_sys::base::{CFGetTypeID, CFRelease, CFTypeRef, kCFAllocatorDefault};
use core_foundation_sys::dictionary::{CFDictionaryGetValue, CFDictionaryRef};
use core_foundation_sys::number::{CFBooleanGetTypeID, CFBooleanGetValue, CFBooleanRef};
use core_foundation_sys::string::{CFStringGetCString, CFStringGetTypeID, CFStringRef, kCFStringEncodingUTF8};

use super::Volume;
use super::mac::{DiskInfo, MacMount, bsd_name, mac_volume};

type DASessionRef = *mut c_void;
type DADiskRef = *mut c_void;

#[link(name = "DiskArbitration", kind = "framework")]
unsafe extern "C" {
    fn DASessionCreate(allocator: *const c_void) -> DASessionRef;
    fn DADiskCreateFromBSDName(allocator: *const c_void, session: DASessionRef, name: *const c_char) -> DADiskRef;
    fn DADiskCopyDescription(disk: DADiskRef) -> CFDictionaryRef;
    static kDADiskDescriptionVolumeNameKey: CFStringRef;
    static kDADiskDescriptionDeviceInternalKey: CFStringRef;
    static kDADiskDescriptionMediaEjectableKey: CFStringRef;
    static kDADiskDescriptionMediaRemovableKey: CFStringRef;
    static kDADiskDescriptionDeviceModelKey: CFStringRef;
    static kDADiskDescriptionDeviceProtocolKey: CFStringRef;
}

pub(super) fn read() -> Vec<Volume> {
    let session = Session::new();
    mounts()
        .iter()
        .filter_map(|m| {
            let local = bsd_name(&m.from).and_then(|n| session.as_ref().and_then(|s| s.describe(n)));
            let mut disk = local;
            if m.path == "/" && disk.as_ref().and_then(|d| d.name.as_ref()).is_none() {
                // The boot volume's name, from Finder's `/Volumes/<name>` link to `/`.
                disk.get_or_insert_with(DiskInfo::default).name = boot_link_name();
            }
            mac_volume(m, disk.as_ref())
        })
        .collect()
}

fn mounts() -> Vec<MacMount> {
    // SAFETY: a null buffer asks only for the count.
    let n = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if n <= 0 {
        return Vec::new();
    }
    // Room for a few mounts that arrive between the two calls.
    let cap = n as usize + 8;
    let mut buf: Vec<libc::statfs> = Vec::with_capacity(cap);
    let bytes = (cap * std::mem::size_of::<libc::statfs>()) as libc::c_int;
    // SAFETY: `buf` has room for `cap` entries; getfsstat writes at most `bytes`.
    let got = unsafe { libc::getfsstat(buf.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
    if got <= 0 {
        return Vec::new();
    }
    // SAFETY: getfsstat initialised `got` entries (never more than `cap`).
    unsafe { buf.set_len((got as usize).min(cap)) };
    buf.iter()
        .map(|s| {
            let bs = u64::from(s.f_bsize);
            MacMount {
                path: c_str(&s.f_mntonname),
                from: c_str(&s.f_mntfromname),
                fstype: c_str(&s.f_fstypename),
                flags: s.f_flags,
                total: s.f_blocks.saturating_mul(bs),
                free: s.f_bavail.saturating_mul(bs),
            }
        })
        .collect()
}

fn c_str(b: &[c_char]) -> String {
    // SAFETY: the kernel NUL-terminates these fixed buffers; the slice bounds the read anyway.
    let bytes: &[u8] = unsafe { std::slice::from_raw_parts(b.as_ptr().cast(), b.len()) };
    CStr::from_bytes_until_nul(bytes).map(|c| c.to_string_lossy().into_owned()).unwrap_or_default()
}

fn boot_link_name() -> Option<String> {
    std::fs::read_dir("/Volumes").ok()?.flatten().find_map(|e| {
        let target = std::fs::read_link(e.path()).ok()?;
        (target == Path::new("/")).then(|| e.file_name().to_string_lossy().into_owned())
    })
}

/// A DiskArbitration session, released on drop.
struct Session(DASessionRef);

impl Session {
    fn new() -> Option<Session> {
        // SAFETY: plain constructor; null on failure.
        let s = unsafe { DASessionCreate(kCFAllocatorDefault.cast()) };
        (!s.is_null()).then_some(Session(s))
    }

    fn describe(&self, bsd: &str) -> Option<DiskInfo> {
        let name = std::ffi::CString::new(bsd).ok()?;
        // SAFETY: valid session and NUL-terminated name; the disk and its description are
        // CF objects we own (Create/Copy rule) and release below.
        unsafe {
            let disk = DADiskCreateFromBSDName(kCFAllocatorDefault.cast(), self.0, name.as_ptr());
            if disk.is_null() {
                return None;
            }
            let desc = DADiskCopyDescription(disk);
            CFRelease(disk as CFTypeRef);
            if desc.is_null() {
                return None;
            }
            let info = DiskInfo {
                name: string(desc, kDADiskDescriptionVolumeNameKey),
                internal: boolean(desc, kDADiskDescriptionDeviceInternalKey),
                ejectable: boolean(desc, kDADiskDescriptionMediaEjectableKey).unwrap_or(false)
                    || boolean(desc, kDADiskDescriptionMediaRemovableKey).unwrap_or(false),
                model: string(desc, kDADiskDescriptionDeviceModelKey).map(|m| m.trim().to_owned()),
                protocol: string(desc, kDADiskDescriptionDeviceProtocolKey),
            };
            CFRelease(desc as CFTypeRef);
            Some(info)
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: created by DASessionCreate, released once.
        unsafe { CFRelease(self.0 as CFTypeRef) };
    }
}

/// SAFETY: `desc` is a live CFDictionary and `key` a CFString.
unsafe fn value(desc: CFDictionaryRef, key: CFStringRef) -> CFTypeRef {
    unsafe { CFDictionaryGetValue(desc, key as *const c_void) }
}

/// SAFETY: as [`value`].
unsafe fn boolean(desc: CFDictionaryRef, key: CFStringRef) -> Option<bool> {
    unsafe {
        let v = value(desc, key);
        (!v.is_null() && CFGetTypeID(v) == CFBooleanGetTypeID()).then(|| CFBooleanGetValue(v as CFBooleanRef))
    }
}

/// SAFETY: as [`value`].
unsafe fn string(desc: CFDictionaryRef, key: CFStringRef) -> Option<String> {
    unsafe {
        let v = value(desc, key);
        if v.is_null() || CFGetTypeID(v) != CFStringGetTypeID() {
            return None;
        }
        let mut buf = [0 as c_char; 512];
        let ok = CFStringGetCString(v as CFStringRef, buf.as_mut_ptr(), buf.len() as _, kCFStringEncodingUTF8);
        (ok != 0).then(|| c_str(&buf))
    }
}
