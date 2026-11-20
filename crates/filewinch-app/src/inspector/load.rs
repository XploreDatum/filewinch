//! The Inspector's background work: the dates `Entry` doesn't carry, previews and folder sizes.
//! Plain functions of a remote and a path, run off the UI thread (see `mod.rs`). Each checks a
//! [`Cancel`] flag between reads, so a selection change stops a download or a folder walk
//! mid-way, and retries a transient failure (a dropped connection, a rate limit) with backoff.

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use filewinch_core::{Entry, EntryKind, Error, ErrorKind, RPath, Remote, Result};

use crate::icons::FileKind;
use crate::pane::model::file_kind;

/// Larger files are never fetched for a preview without asking ("Download to preview").
pub const PREVIEW_MAX: u64 = 20 * 1024 * 1024;
/// How much of a text file the preview shows.
pub const TEXT_MAX: usize = 64 * 1024;
/// A local folder with at most this many items inside is sized without asking.
pub const AUTO_SIZE_ITEMS: u64 = 2_000;

/// Stops a piece of background work. Clones share the flag.
#[derive(Clone, Default, Debug)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
    fn check(&self) -> Result<()> {
        if self.is_cancelled() { Err(Error::cancelled()) } else { Ok(()) }
    }
}

/// Runs `f`, again after 0.3 s and 0.9 s when it fails with a transient error (a dropped
/// connection, a rate limit), unless cancelled meanwhile.
pub fn retrying<T>(cancel: &Cancel, mut f: impl FnMut() -> Result<T>) -> Result<T> {
    let mut wait = Duration::from_millis(300);
    for _ in 0..2 {
        match f() {
            Err(e) if e.kind.is_transient() && !cancel.is_cancelled() => {
                std::thread::sleep(e.retry_after.unwrap_or(wait).min(Duration::from_secs(5)));
                wait *= 3;
                cancel.check()?;
            }
            other => return other,
        }
    }
    f()
}

/// When an item was made and last read. Only this computer says (servers don't report them);
/// `None` reads "n/a".
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Dates {
    pub created: Option<SystemTime>,
    pub accessed: Option<SystemTime>,
}

/// The dates of `path` on this computer; none when the file system doesn't keep them (or the
/// item went away).
pub fn local_dates(path: &RPath) -> Dates {
    let Ok(m) = std::fs::symlink_metadata(filewinch_local::local_path(path)) else { return Dates::default() };
    let sane = |t: std::io::Result<SystemTime>| t.ok().filter(|t| *t > std::time::UNIX_EPOCH);
    Dates { created: sane(m.created()), accessed: sane(m.accessed()) }
}

/// What a file's preview shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewKind {
    Image,
    Text,
}

/// The preview `e` gets, if any: images Rune decodes, and text (source, plain text, or a file
/// of unknown type whose first bytes turn out to be text).
pub fn preview_kind(e: &Entry) -> Option<PreviewKind> {
    if e.kind != EntryKind::File {
        return None;
    }
    let ext = e.name.rsplit_once('.').map_or(String::new(), |(_, x)| x.to_ascii_lowercase());
    match file_kind(e) {
        FileKind::Image if matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "ico" | "bmp" | "avif") => Some(PreviewKind::Image),
        FileKind::Text | FileKind::Code | FileKind::Other => Some(PreviewKind::Text),
        _ => None,
    }
}

/// A finished preview fetch.
#[derive(Clone, Debug, PartialEq)]
pub enum Fetched {
    Image(Arc<[u8]>),
    Text { text: String, truncated: bool },
    /// The file isn't text after all.
    Binary,
}

/// Reads up to `max` bytes from `r`, checking `cancel` between chunks and counting into `got`.
fn read_capped(mut r: impl Read, max: u64, cancel: &Cancel, got: &AtomicU64) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    while (out.len() as u64) < max {
        cancel.check()?;
        let want = buf.len().min((max - out.len() as u64) as usize);
        let n = r.read(&mut buf[..want]).map_err(Error::from)?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        got.fetch_add(n as u64, Ordering::Relaxed);
    }
    Ok(out)
}

/// The first [`TEXT_MAX`] bytes as text, or `Binary` when they hold NULs or aren't UTF-8. A
/// character cut by the limit is dropped rather than shown as a replacement mark.
pub fn as_text(bytes: &[u8], file_len: Option<u64>) -> Fetched {
    let head = &bytes[..bytes.len().min(TEXT_MAX)];
    if head.iter().take(8192).any(|b| *b == 0) {
        return Fetched::Binary;
    }
    let text = match std::str::from_utf8(head) {
        Ok(t) => t,
        // Cut mid-character at the limit: keep what's whole.
        Err(e) if e.error_len().is_none() => std::str::from_utf8(&head[..e.valid_up_to()]).unwrap_or_default(),
        Err(_) => return Fetched::Binary,
    };
    let truncated = file_len.map_or(bytes.len() > TEXT_MAX, |n| n > head.len() as u64);
    Fetched::Text { text: text.to_owned(), truncated }
}

/// Where a remote image's preview copy lives: `<cache>/preview/<session>/<path>`.
pub fn cache_file(session: u64, path: &RPath) -> PathBuf {
    crate::edit_watch::cache_dir("preview").join(session.to_string()).join(path.as_str().trim_start_matches('/'))
}

/// Fetches `e`'s preview. Text reads the first 64 KB. An image reads the whole file (refused
/// over [`PREVIEW_MAX`] unless `force`); a remote one goes through `cache` (a copy that's the
/// same size and no older than the server's is used as is), written beside it and renamed into
/// place once whole, so a cancelled download never leaves a cut-off copy behind.
pub fn fetch(remote: &dyn Remote, e: &Entry, kind: PreviewKind, cache: Option<PathBuf>, force: bool, cancel: &Cancel, got: &AtomicU64) -> Result<Fetched> {
    match kind {
        PreviewKind::Text => {
            let bytes = retrying(cancel, || {
                got.store(0, Ordering::Relaxed);
                read_capped(remote.read(&e.path, 0)?, TEXT_MAX as u64 + 1, cancel, got)
            })?;
            Ok(as_text(&bytes, e.size))
        }
        PreviewKind::Image => {
            if !force && e.size.is_some_and(|n| n > PREVIEW_MAX) {
                return Err(too_large());
            }
            let cap = if force { u64::MAX } else { PREVIEW_MAX + 1 };
            if let Some(file) = &cache
                && let Some(bytes) = cached(file, e)
            {
                got.store(bytes.len() as u64, Ordering::Relaxed);
                return Ok(Fetched::Image(bytes.into()));
            }
            let bytes = retrying(cancel, || {
                got.store(0, Ordering::Relaxed);
                read_capped(remote.read(&e.path, 0)?, cap, cancel, got)
            })?;
            if !force && bytes.len() as u64 > PREVIEW_MAX {
                return Err(too_large());
            }
            if let Some(file) = cache {
                // A copy for next time; failing to keep one only costs a download later.
                let _ = store(&file, &bytes);
            }
            Ok(Fetched::Image(bytes.into()))
        }
    }
}

fn too_large() -> Error {
    Error::new(ErrorKind::Unsupported, format!("Larger than {} — not downloaded for a preview.", crate::fmt::size(PREVIEW_MAX)))
}

fn cached(file: &std::path::Path, e: &Entry) -> Option<Vec<u8>> {
    let m = std::fs::metadata(file).ok()?;
    let fresh = e.modified.is_none_or(|server| m.modified().is_ok_and(|ours| ours >= server));
    (Some(m.len()) == e.size && fresh).then(|| std::fs::read(file).ok()).flatten()
}

fn store(file: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = file.parent().ok_or_else(|| std::io::Error::other("no folder"))?;
    std::fs::create_dir_all(dir)?;
    let part = dir.join(format!(".{}.filewinch-preview", file.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())));
    std::fs::write(&part, bytes)?;
    std::fs::rename(&part, file)
}

/// Running totals of a folder walk, read by the UI while it counts.
#[derive(Default, Debug)]
pub struct Tally {
    pub bytes: AtomicU64,
    pub files: AtomicU64,
    pub folders: AtomicU64,
}

impl Tally {
    pub fn get(&self) -> (u64, u64, u64) {
        (self.bytes.load(Ordering::Relaxed), self.files.load(Ordering::Relaxed), self.folders.load(Ordering::Relaxed))
    }
}

/// A finished folder walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sized {
    Done { bytes: u64, files: u64, folders: u64 },
    /// More than the `cap` items: left for the person to ask for.
    TooMany,
}

/// Bytes, files and folders under `roots` (walked iteratively, so depth doesn't matter), and
/// `extra` files counted as given. With `cap`, stops at that many items with `TooMany`.
pub fn folder_size(remote: &dyn Remote, roots: &[RPath], cancel: &Cancel, tally: &Tally, cap: Option<u64>) -> Result<Sized> {
    let mut stack = roots.to_vec();
    while let Some(d) = stack.pop() {
        cancel.check()?;
        let listed = retrying(cancel, || remote.list(&d))?;
        for e in listed {
            match e.kind {
                EntryKind::Dir => {
                    tally.folders.fetch_add(1, Ordering::Relaxed);
                    stack.push(e.path);
                }
                _ => {
                    tally.bytes.fetch_add(e.size.unwrap_or(0), Ordering::Relaxed);
                    tally.files.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        let (_, files, folders) = tally.get();
        if cap.is_some_and(|c| files + folders > c) {
            return Ok(Sized::TooMany);
        }
    }
    let (bytes, files, folders) = tally.get();
    Ok(Sized::Done { bytes, files, folders })
}
