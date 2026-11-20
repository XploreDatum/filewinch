//! Filewinch's protocol contract. Every backend (local disk, SFTP, FTP/FTPS, WebDAV, S3,
//! Azure, Swift, Google Drive, Dropbox, OneDrive, Box) implements [`Remote`]; the transfer
//! engine and the UI only ever talk to this trait.
//!
//! The trait is blocking on purpose: the UI runs backends on worker threads and wakes the
//! Rune runtime when results land. Backends built on async libraries own a private runtime.

mod error;
mod path;
mod secrets;
mod site;
mod trust;

pub use error::{Error, ErrorKind, Result};
pub use path::RPath;
pub use secrets::{FnSecrets, MemorySecrets, Secrets};
pub use site::{Auth, Protocol, SecretRef, Site, TlsMode};
pub use trust::{fingerprint_from_sha256, is_trusted, normalize_fingerprint};

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::SystemTime;

/// What a backend can do. The UI hides or disables actions a backend lacks, and the transfer
/// engine only resumes where `resume_read`/`resume_write` say it can.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Caps {
    pub resume_read: bool,
    pub resume_write: bool,
    pub permissions: bool,
    pub set_mtime: bool,
    pub symlinks: bool,
    /// Directories are real objects. False for flat object stores (S3, Azure, Swift), where a
    /// "folder" is a key prefix and `mkdir` writes a zero-byte `name/` marker.
    pub real_dirs: bool,
    /// Rename is server-side. False means the engine must copy then delete.
    pub server_rename: bool,
    /// Server-side copy within the same remote.
    pub server_copy: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// Final path component, never empty.
    pub name: String,
    pub path: RPath,
    pub kind: EntryKind,
    /// None when the server doesn't say (folders on most servers).
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    /// Unix mode bits when the protocol has them.
    pub mode: Option<u32>,
    pub owner: Option<String>,
    pub group: Option<String>,
    /// Where a symlink points, if known.
    pub link_target: Option<String>,
    /// Content type or ETag-like extras a backend wants to show in Get Info.
    pub extra: Vec<(String, String)>,
    /// Content hashes the server reported with the listing (files only; empty when it gives
    /// none, or when its "hash" isn't a hash of the bytes, e.g. a multipart S3 ETag).
    pub checksums: Vec<Checksum>,
    /// Set when other people can see this item through a share (a shared folder you joined or
    /// own, a "Shared with me" item). Deleting the item's contents would delete them for
    /// everyone, so the engine never recurses into it; see [`Shared`] and
    /// [`Remote::leave_shared`]. None for ordinary items and on backends without sharing.
    pub shared: Option<Shared>,
}

/// How an item is shared, and what removing it from this account can mean.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Shared {
    /// The signed-in account owns the item and shared it with others. Deleting it deletes it
    /// for everyone; an owner usually can't "leave" their own share.
    pub owned_by_me: bool,
    /// [`Remote::leave_shared`] can take the item out of this account only, leaving it (and
    /// everything in it) untouched for the people it's shared with.
    pub can_leave: bool,
}

/// Hash algorithms servers report. Sync's compare-by-checksum hashes local bytes with whichever
/// of these the other side offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HashAlgo {
    Md5,
    Sha1,
    Sha256,
    /// Dropbox `content_hash`: SHA-256 of the concatenated SHA-256s of each 4 MiB block.
    DropboxContent,
    /// OneDrive `quickXorHash` (shown here as hex, not base64).
    QuickXor,
}

/// One server-reported content hash. `hex` is lower-case hexadecimal.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Checksum {
    pub algo: HashAlgo,
    pub hex: String,
}

impl Checksum {
    /// Lower-cases `hex`; `None` if it isn't a non-empty hex string.
    pub fn new(algo: HashAlgo, hex: &str) -> Option<Checksum> {
        let hex = hex.trim().to_ascii_lowercase();
        (!hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(Checksum { algo, hex })
    }

    /// From raw digest bytes (for servers that send base64, like Azure's Content-MD5).
    pub fn from_bytes(algo: HashAlgo, bytes: &[u8]) -> Checksum {
        Checksum { algo, hex: bytes.iter().map(|b| format!("{b:02x}")).collect() }
    }
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }
}

/// A file being written. Bytes go in through `Write`; `finish` commits (closes the SFTP handle,
/// completes an S3 multipart upload, finalises a resumable Drive session). Dropping without
/// `finish` aborts: the backend cleans up what it can and leaves no half-committed object
/// where the protocol allows that.
pub trait WriteSink: Write + Send {
    fn finish(self: Box<Self>) -> Result<()>;
}

/// One connected session. Implementations are `Send + Sync`; the engine may call from several
/// threads, and a backend that needs one connection per transfer opens it internally (pooled).
pub trait Remote: Send + Sync {
    fn caps(&self) -> Caps;

    /// The directory a session opens in (the SFTP/FTP home, a bucket root, Drive "My Drive").
    fn home(&self) -> Result<RPath>;

    /// Lists a directory's direct children, without `.` and `..`.
    fn list(&self, dir: &RPath) -> Result<Vec<Entry>>;

    /// Doesn't follow symlinks (like `lstat`): a link comes back as `EntryKind::Symlink` with
    /// `link_target` filled when the protocol can tell (a relative target is relative to the
    /// link's folder). The engine relies on this so a recursive delete never walks into a
    /// linked folder.
    fn stat(&self, path: &RPath) -> Result<Entry>;

    /// Opens a file for reading starting at `offset` (0 unless `caps().resume_read`).
    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>>;

    /// Opens a file for writing. `offset > 0` appends to an existing partial file and is only
    /// passed when `caps().resume_write`. `size` is the total final size when known, so object
    /// stores can choose single-put or multipart.
    fn write(&self, path: &RPath, offset: u64, size: Option<u64>) -> Result<Box<dyn WriteSink>>;

    fn mkdir(&self, path: &RPath) -> Result<()>;

    /// Moves or renames within this remote. Fails with `Unsupported` if `!caps().server_rename`.
    fn rename(&self, from: &RPath, to: &RPath) -> Result<()>;

    /// Deletes a file or symlink.
    fn remove_file(&self, path: &RPath) -> Result<()>;

    /// Deletes an empty directory. The engine handles recursion.
    fn remove_dir(&self, path: &RPath) -> Result<()>;

    /// Server-side copy. Default: unsupported.
    fn copy(&self, _from: &RPath, _to: &RPath) -> Result<()> {
        Err(Error::unsupported("server-side copy"))
    }

    /// Removes a shared item (one with [`Entry::shared`] whose `can_leave` is true) from this
    /// account only: leaves the shared folder, deletes the shortcut, drops the collaboration.
    /// Nothing inside it is deleted for the other people. Default: unsupported.
    fn leave_shared(&self, _path: &RPath) -> Result<()> {
        Err(Error::unsupported("leaving shared items"))
    }

    fn set_mode(&self, _path: &RPath, _mode: u32) -> Result<()> {
        Err(Error::unsupported("permissions"))
    }

    fn set_mtime(&self, _path: &RPath, _mtime: SystemTime) -> Result<()> {
        Err(Error::unsupported("modification time"))
    }

    /// Closes connections. Called when the user disconnects; calls after it fail with `Closed`.
    fn close(&self) {}
}

/// Opens a [`Remote`] for a saved [`Site`]. Each protocol crate exports one; the app keeps a
/// table from [`Protocol`] to connector. `secrets` resolves a [`SecretRef`] (Keychain) to the
/// password, key passphrase or OAuth refresh token; a session may keep it to save a rotated
/// refresh token later ([`Secrets::set`]).
///
/// Trust: a backend that checks a host key or TLS certificate accepts any fingerprint in
/// `site.trusted_fingerprints` (compare with [`is_trusted`]); otherwise it fails with
/// [`Error::host_trust`], whose `detail` starts with the canonical fingerprint.
pub trait Connector: Send + Sync {
    fn protocols(&self) -> &'static [Protocol];
    fn connect(&self, site: &Site, secrets: Arc<dyn Secrets>) -> Result<Box<dyn Remote>>;
}
