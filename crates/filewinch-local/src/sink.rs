//! Writing a local file: in place for new files and resumes, through a hidden sibling for
//! overwrites, so a cancelled overwrite never destroys the old file. On Windows the sibling
//! also gets the hidden attribute, since Explorer doesn't hide dot files.

use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use filewinch_core::{Error, ErrorKind, Result, WriteSink};

use crate::fsx::io_error;

pub struct LocalSink {
    file: Option<File>,
    /// Where the bytes go now.
    writing: PathBuf,
    /// Set when writing a sibling that replaces `target` on finish.
    target: Option<PathBuf>,
    /// True when `open` created the file (nothing was at the path before).
    pub created: bool,
}

impl LocalSink {
    pub fn open(path: &Path, offset: u64) -> Result<LocalSink> {
        if let Ok(md) = fs::symlink_metadata(path)
            && md.is_dir()
        {
            let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            return Err(Error::new(ErrorKind::InvalidPath, format!("Couldn't write “{name}”: a folder has that name.")));
        }
        if offset > 0 {
            let mut file = OpenOptions::new().write(true).open(path).map_err(|e| io_error(e, "resume writing", path))?;
            let len = file.metadata().map_err(|e| io_error(e, "resume writing", path))?.len();
            if len < offset {
                return Err(Error::new(
                    ErrorKind::InvalidPath,
                    format!("Couldn't resume “{}”: the partial file is shorter than expected.", path.display()),
                ));
            }
            file.set_len(offset).map_err(|e| io_error(e, "resume writing", path))?;
            file.seek(SeekFrom::Start(offset)).map_err(|e| io_error(e, "resume writing", path))?;
            return Ok(LocalSink { file: Some(file), writing: path.to_owned(), target: None, created: false });
        }
        if fs::symlink_metadata(path).is_ok() {
            let sibling = sibling_for(path);
            let file = File::create(&sibling).map_err(|e| io_error(e, "write", path))?;
            #[cfg(windows)]
            crate::sys::set_hidden(&sibling, true);
            return Ok(LocalSink { file: Some(file), writing: sibling, target: Some(path.to_owned()), created: false });
        }
        let file = File::create(path).map_err(|e| io_error(e, "write", path))?;
        Ok(LocalSink { file: Some(file), writing: path.to_owned(), target: None, created: true })
    }
}

/// `.name.filewinch-<pid>-<n>` beside `path`.
fn sibling_for(path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let n = N.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(".{name}.filewinch-{}-{n}", std::process::id()))
}

impl Write for LocalSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.file.as_mut().ok_or_else(|| std::io::Error::other("already finished"))?.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.as_mut().map_or(Ok(()), Write::flush)
    }
}

impl WriteSink for LocalSink {
    fn finish(mut self: Box<Self>) -> Result<()> {
        let Some(mut file) = self.file.take() else { return Ok(()) };
        let path = self.target.clone().unwrap_or_else(|| self.writing.clone());
        file.flush().and_then(|()| file.sync_all()).map_err(|e| io_error(e, "save", &path))?;
        drop(file);
        if let Some(target) = self.target.take() {
            // Keep the replaced file's permissions, as an editor's save does.
            if let Ok(md) = fs::metadata(&target) {
                let _ = fs::set_permissions(&self.writing, md.permissions());
                // The sibling was hidden only while being written.
                #[cfg(windows)]
                crate::sys::set_hidden(&self.writing, crate::sys::is_hidden(&md));
            }
            if let Err(e) = fs::rename(&self.writing, &target) {
                #[cfg(windows)]
                crate::sys::clear_readonly(&self.writing);
                let _ = fs::remove_file(&self.writing);
                return Err(replace_error(e, &target));
            }
        }
        Ok(())
    }
}

/// Why replacing `target` failed. Windows refuses to replace a file that is read-only or
/// open in another app; say so instead of a bare "access denied".
fn replace_error(e: std::io::Error, target: &Path) -> Error {
    if cfg!(windows) && e.raw_os_error().is_some_and(crate::winpath::replace_blocked) {
        let name = target.file_name().map_or_else(|| target.display().to_string(), |n| n.to_string_lossy().into_owned());
        return Error::new(ErrorKind::PermissionDenied, format!("Couldn't replace “{name}”: it's open in another app or read-only."))
            .with_detail(format!("{}: {e}", target.display()));
    }
    io_error(e, "replace", target)
}

impl Drop for LocalSink {
    fn drop(&mut self) {
        // Not finished: an overwrite's sibling goes (the old file stays); a new or resumed
        // file keeps its partial bytes so the transfer can resume.
        if self.file.take().is_some() && self.target.is_some() {
            let _ = fs::remove_file(&self.writing);
        }
    }
}
