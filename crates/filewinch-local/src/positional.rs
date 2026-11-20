//! A file filled in pieces from several threads at once (segmented downloads): positional
//! writes (`pwrite` on macOS and Linux, `WriteFile` at an offset on Windows), with the space
//! reserved up front where the file system can, so a full disk fails at the start instead of
//! halfway through.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use filewinch_core::{Error, ErrorKind, PositionalSink, Result};

use crate::fsx::io_error;

pub struct LocalPositional {
    file: File,
    path: PathBuf,
}

impl LocalPositional {
    /// Opens (or creates) `path` without truncating it and makes it exactly `size` bytes long.
    pub fn open(path: &Path, size: u64) -> Result<LocalPositional> {
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
            let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            return Err(Error::new(ErrorKind::InvalidPath, format!("Couldn't write “{name}”: a folder has that name.")));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| io_error(e, "write", path))?;
        #[cfg(windows)]
        if path.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
            crate::sys::set_hidden(path, true);
        }
        let len = file.metadata().map_err(|e| io_error(e, "write", path))?.len();
        if len < size {
            reserve(&file, len, size).map_err(|e| no_space(e, path, size))?;
        }
        if len != size {
            file.set_len(size).map_err(|e| no_space(e, path, size))?;
        }
        Ok(LocalPositional { file, path: path.to_owned() })
    }
}

fn no_space(e: std::io::Error, path: &Path, size: u64) -> Error {
    if matches!(e.kind(), std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded) {
        let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let mb = size.div_ceil(1_000_000);
        return Error::new(ErrorKind::NoSpace, format!("There isn't enough free space for “{name}” ({mb} MB).")).with_detail(e);
    }
    io_error(e, "write", path)
}

/// Reserves blocks for bytes `from..to`. Only "out of space" is an error; a file system that
/// can't reserve (or a sparse-only one) just gets the length set.
#[cfg(target_os = "macos")]
fn reserve(file: &File, from: u64, to: u64) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let mut store = libc::fstore_t {
        fst_flags: libc::F_ALLOCATEALL,
        fst_posmode: libc::F_PEOFPOSMODE,
        fst_offset: 0,
        fst_length: (to - from) as libc::off_t,
        fst_bytesalloc: 0,
    };
    // SAFETY: a valid descriptor and a properly initialised fstore_t that outlives the call.
    let r = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PREALLOCATE, &mut store) };
    match r {
        -1 => match std::io::Error::last_os_error() {
            e if e.raw_os_error() == Some(libc::ENOSPC) => Err(e),
            _ => Ok(()),
        },
        _ => Ok(()),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn reserve(file: &File, from: u64, to: u64) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: a valid descriptor; posix_fallocate only reads its integer arguments.
    let r = unsafe { libc::posix_fallocate(file.as_raw_fd(), from as libc::off_t, (to - from) as libc::off_t) };
    match r {
        libc::ENOSPC => Err(std::io::Error::from_raw_os_error(r)),
        _ => Ok(()),
    }
}

/// NTFS allocates when the length is set.
#[cfg(windows)]
fn reserve(_file: &File, _from: u64, _to: u64) -> std::io::Result<()> {
    Ok(())
}

impl PositionalSink for LocalPositional {
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        write_all_at(&self.file, offset, buf).map_err(|e| io_error(e, "write", &self.path))
    }

    fn sync(&self) -> Result<()> {
        self.file.sync_data().map_err(|e| io_error(e, "save", &self.path))
    }

    fn finish(self: Box<Self>) -> Result<()> {
        self.file.sync_all().map_err(|e| io_error(e, "save", &self.path))
    }
}

#[cfg(unix)]
fn write_all_at(f: &File, offset: u64, buf: &[u8]) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(f, buf, offset)
}

#[cfg(windows)]
fn write_all_at(f: &File, mut offset: u64, mut buf: &[u8]) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match f.seek_write(buf, offset) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
