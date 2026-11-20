//! "This Mac" / "This Computer": the [`Remote`] contract over the local disk on macOS, Linux
//! and Windows.
//!
//! On macOS and Linux an [`RPath`] is the absolute path (`/Users/me/Sites`). On Windows it
//! stays `/`-separated with the drive first: `/C:/Users/me` is `C:\Users\me`,
//! `/UNC/server/share/x` is `\\server\share\x`, and the root `/` lists the drives (see
//! [`windows_path`]). Names Windows refuses (`12:30 report?.txt`) are saved under a
//! look-alike (`12：30 report？.txt`) and reported through [`take_renamed`].
//!
//! Writes that replace an existing file go through a hidden sibling and are renamed into
//! place on `finish`, so a cancelled or failed overwrite leaves the old file untouched; a new
//! file is written in place, so a failed download leaves its partial bytes for the engine to
//! resume.
//!
//! Unix mode bits, owners and groups are reported on macOS and Linux only. Symbolic links are
//! listed on every OS; on Windows an item with the hidden attribute carries [`HIDDEN_EXTRA`]
//! (see [`is_hidden`]).

mod fsx;
mod sink;
#[cfg(test)]
mod tests;
#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as sys;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as sys;
mod winpath;
#[cfg(test)]
mod winpath_tests;
#[cfg(all(test, windows))]
mod windows_tests;

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use std::sync::Arc;

use filewinch_core::{
    Caps, Connector, Entry, Error, ErrorKind, Protocol, RPath, Remote, Result, Secrets, Site, WriteSink,
};

pub use fsx::io_error;
pub use winpath::{RENAMED_CAP, Renamed, is_volume_root, rpath_from_windows, take_renamed, windows_path};

/// The [`Entry::extra`] pair marking an item with Windows' hidden attribute.
pub const HIDDEN_EXTRA: (&str, &str) = ("Hidden", "Yes");

/// True for an item the file browser hides by default: a dot file on every OS, or an item
/// with Windows' hidden attribute.
pub fn is_hidden(e: &Entry) -> bool {
    e.name.starts_with('.') || e.extra.iter().any(|(k, v)| k == HIDDEN_EXTRA.0 && v == HIDDEN_EXTRA.1)
}

/// Opens [`LocalRemote`] sessions for [`Protocol::Local`].
pub struct LocalConnector;

impl Connector for LocalConnector {
    fn protocols(&self) -> &'static [Protocol] {
        &[Protocol::Local]
    }

    fn connect(&self, _site: &Site, _secrets: Arc<dyn Secrets>) -> Result<Box<dyn Remote>> {
        Ok(Box::new(LocalRemote::new()))
    }
}

/// The local disk as a [`Remote`].
#[derive(Default)]
pub struct LocalRemote {
    closed: AtomicBool,
}

impl LocalRemote {
    pub fn new() -> Self {
        Self::default()
    }

    fn open(&self) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            let this = filewinch_os::Os::CURRENT.this_computer();
            return Err(Error::new(ErrorKind::Closed, format!("{this}'s session was closed.")));
        }
        Ok(())
    }
}

/// The path on this computer for a remote path. On Windows a path that names no place on
/// disk (the drive list `/`, `/UNC/server`) gives an empty path.
pub fn local_path(p: &RPath) -> PathBuf {
    if cfg!(windows) { windows_path(p).map(PathBuf::from).unwrap_or_default() } else { PathBuf::from(p.as_str()) }
}

/// The remote path for a path on this computer. On macOS and Linux relative paths are taken
/// from `/`; on Windows they are resolved against the current folder first, and `C:\x`,
/// `C:/x`, `\\?\C:\x` and `\\server\share\x` are all accepted.
pub fn rpath(p: &Path) -> RPath {
    if cfg!(windows) {
        let abs = if p.is_absolute() { p.to_owned() } else { std::path::absolute(p).unwrap_or_else(|_| p.to_owned()) };
        rpath_from_windows(&abs.to_string_lossy())
    } else {
        RPath::new(&p.to_string_lossy())
    }
}

/// The disk path for `p`, or a clear error when it names no place on disk (Windows only).
fn disk(p: &RPath) -> Result<PathBuf> {
    if !cfg!(windows) {
        return Ok(PathBuf::from(p.as_str()));
    }
    windows_path(p).map(PathBuf::from).ok_or_else(|| {
        Error::new(ErrorKind::InvalidPath, format!("“{p}” isn't a folder on this computer. Open a drive such as C: first."))
    })
}

/// The disk path for an item about to be created, renamed or deleted. On Windows the drive
/// list and the drives themselves can't be changed.
fn changeable(p: &RPath, verb: &str) -> Result<PathBuf> {
    if cfg!(windows) {
        if p.is_root() {
            return Err(Error::new(
                ErrorKind::InvalidPath,
                format!("Couldn't {verb} here: the top level lists this computer's drives. Open a drive first."),
            ));
        }
        if is_volume_root(p) {
            return Err(Error::new(
                ErrorKind::InvalidPath,
                format!("Couldn't {verb} “{}”: it's a drive. Work inside the drive instead.", p.name()),
            ));
        }
    }
    disk(p)
}

/// Records that `p` was just created under a Windows-safe name, if it was.
fn note_created(p: &RPath) {
    if cfg!(windows)
        && let Some(r) = winpath::substitution(p)
    {
        winpath::record(r);
    }
}

impl Remote for LocalRemote {
    fn caps(&self) -> Caps {
        Caps {
            resume_read: true,
            resume_write: true,
            permissions: cfg!(unix),
            set_mtime: true,
            symlinks: true,
            real_dirs: true,
            server_rename: true,
            server_copy: true,
        }
    }

    fn home(&self) -> Result<RPath> {
        self.open()?;
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let home = filewinch_os::dirs::home()
            .ok_or_else(|| Error::new(ErrorKind::NotFound, format!("Your home folder couldn't be found ({var} isn't set).")))?;
        Ok(rpath(&home))
    }

    fn list(&self, dir: &RPath) -> Result<Vec<Entry>> {
        self.open()?;
        #[cfg(windows)]
        if dir.is_root() {
            return sys::drive_entries();
        }
        let path = disk(dir)?;
        let rd = fs::read_dir(&path).map_err(|e| io_error(e, "open the folder", &path))?;
        let mut names = fsx::Names::default();
        let mut out = Vec::new();
        for item in rd {
            let item = item.map_err(|e| io_error(e, "read the folder", &path))?;
            let name = item.file_name().to_string_lossy().into_owned();
            let child = dir.join(&name);
            // An entry deleted between the listing and its stat is simply gone.
            match fs::symlink_metadata(item.path()) {
                Ok(md) => out.push(fsx::entry(name, child, &md, &item.path(), &mut names)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_error(e, "read", &item.path())),
            }
        }
        Ok(out)
    }

    fn stat(&self, path: &RPath) -> Result<Entry> {
        self.open()?;
        if cfg!(windows) && path.is_root() {
            return Ok(winpath::dir_entry("/".to_owned(), RPath::root()));
        }
        let p = disk(path)?;
        let md = fs::symlink_metadata(&p).map_err(|e| io_error(e, "find", &p))?;
        let name = if path.is_root() { "/".to_owned() } else { path.name().to_owned() };
        Ok(fsx::entry(name, path.clone(), &md, &p, &mut fsx::Names::default()))
    }

    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        self.open()?;
        let p = disk(path)?;
        // Windows refuses to open a folder as a file ("access denied"); say what it is.
        if cfg!(windows) && fs::metadata(&p).is_ok_and(|m| m.is_dir()) {
            return Err(Error::new(ErrorKind::InvalidPath, format!("“{}” is a folder, not a file.", path.name())));
        }
        let mut f = fs::File::open(&p).map_err(|e| io_error(e, "open", &p))?;
        if f.metadata().map_err(|e| io_error(e, "read", &p))?.is_dir() {
            return Err(Error::new(ErrorKind::InvalidPath, format!("“{}” is a folder, not a file.", path.name())));
        }
        if offset > 0 {
            f.seek(SeekFrom::Start(offset)).map_err(|e| io_error(e, "resume reading", &p))?;
        }
        Ok(Box::new(f))
    }

    fn write(&self, path: &RPath, offset: u64, _size: Option<u64>) -> Result<Box<dyn WriteSink>> {
        self.open()?;
        let sink = sink::LocalSink::open(&changeable(path, "write")?, offset)?;
        if sink.created {
            note_created(path);
        }
        Ok(Box::new(sink))
    }

    fn mkdir(&self, path: &RPath) -> Result<()> {
        self.open()?;
        let p = changeable(path, "create the folder")?;
        fs::create_dir(&p).map_err(|e| io_error(e, "create the folder", &p))?;
        note_created(path);
        Ok(())
    }

    fn rename(&self, from: &RPath, to: &RPath) -> Result<()> {
        self.open()?;
        fsx::rename_exclusive(&changeable(from, "rename")?, &changeable(to, "rename to")?)
    }

    fn remove_file(&self, path: &RPath) -> Result<()> {
        self.open()?;
        let p = changeable(path, "delete")?;
        let md = fs::symlink_metadata(&p).map_err(|e| io_error(e, "delete", &p))?;
        if md.is_dir() {
            return Err(Error::new(ErrorKind::InvalidPath, format!("“{}” is a folder, not a file.", path.name())));
        }
        // Windows deletes a link to a folder as a folder (the folder it points to stays).
        #[cfg(windows)]
        if sys::is_dir_link(&md) {
            return fs::remove_dir(&p).map_err(|e| io_error(e, "delete", &p));
        }
        fs::remove_file(&p).map_err(|e| io_error(e, "delete", &p))
    }

    fn remove_dir(&self, path: &RPath) -> Result<()> {
        self.open()?;
        let p = changeable(path, "delete the folder")?;
        fs::remove_dir(&p).map_err(|e| io_error(e, "delete the folder", &p))
    }

    fn copy(&self, from: &RPath, to: &RPath) -> Result<()> {
        self.open()?;
        let (src, dst) = (disk(from)?, changeable(to, "copy to")?);
        let md = fs::symlink_metadata(&src).map_err(|e| io_error(e, "copy", &src))?;
        if md.is_dir() {
            return Err(Error::new(ErrorKind::InvalidPath, format!("“{}” is a folder; folders are copied item by item.", from.name())));
        }
        if fs::symlink_metadata(&dst).is_ok() {
            return Err(Error::new(ErrorKind::AlreadyExists, format!("“{}” already exists.", to.name())));
        }
        // On APFS this clones the file (copy-on-write) and keeps its metadata.
        fs::copy(&src, &dst).map(|_| ()).map_err(|e| {
            // Don't leave a half-copied file behind.
            let _ = fs::remove_file(&dst);
            io_error(e, "copy", &src)
        })
    }

    fn set_mode(&self, path: &RPath, mode: u32) -> Result<()> {
        self.open()?;
        #[cfg(windows)]
        {
            let _ = (path, mode);
            Err(Error::new(ErrorKind::Unsupported, "Windows doesn't use Unix permissions."))
        }
        #[cfg(unix)]
        {
            let p = disk(path)?;
            sys::set_mode(&p, mode).map_err(|e| io_error(e, "change permissions of", &p))
        }
    }

    fn set_mtime(&self, path: &RPath, mtime: SystemTime) -> Result<()> {
        self.open()?;
        let p = disk(path)?;
        #[cfg(windows)]
        let f = sys::open_for_dates(&p).map_err(|e| io_error(e, "open", &p))?;
        #[cfg(unix)]
        let f = fs::File::open(&p).map_err(|e| io_error(e, "open", &p))?;
        f.set_modified(mtime).map_err(|e| io_error(e, "set the modification date of", &p))
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
}
