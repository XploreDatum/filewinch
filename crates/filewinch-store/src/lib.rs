//! Filewinch's saved state: Favorites (sites in folders), recent connections and preferences,
//! each a JSON file in the config folder ([`Store::default_dir`]: `~/Library/Application
//! Support/Filewinch` on macOS, `%APPDATA%\Filewinch` on Windows, `$XDG_CONFIG_HOME/filewinch`
//! on Linux), written atomically. Passwords, keys' passphrases and OAuth tokens live in the
//! system's secret store (macOS Keychain, Windows Credential Manager, Secret Service on Linux)
//! through [`SystemSecrets`], never in these files.
//!
//! A file that can't be read is moved aside (`sites.json.corrupt-<time>`) and the app starts
//! from an empty value, with a [`Loaded::warning`] to show the person.

mod prefs;
mod recents;
mod secret_errors;
mod secrets;
mod site_serde;
mod sites;
mod ssh_config;
#[cfg(test)]
mod tests;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use filewinch_core::{Error, ErrorKind, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub use prefs::{Appearance, ColumnId, ColumnPref, ConflictDefault, Prefs};
pub use recents::{Recent, Recents};
pub use secret_errors::NO_SECRET_SERVICE;
/// [`SystemSecrets`] under its original name; both are the same type.
pub use secrets::SystemSecrets as Keychain;
pub use secrets::{MemorySecrets, SecretStore, SystemSecrets, secret_account};
pub use sites::{Favorite, Folder, SitesFile, new_id};
pub use ssh_config::{SshHost, parse_ssh_config, read_ssh_config};

/// The service every secret is stored under in the system's secret store (the Keychain
/// service on macOS). Same as `filewinch_os::APP_ID`.
pub const KEYCHAIN_SERVICE: &str = "app.xpdt.filewinch";

/// A value read from disk, and what to tell the person if it had to start over.
#[derive(Debug)]
pub struct Loaded<T> {
    pub value: T,
    pub warning: Option<String>,
}

/// The folder Filewinch keeps its files in.
#[derive(Clone, Debug)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// This OS's config folder (see `filewinch_os::dirs`): on macOS
    /// `~/Library/Application Support/Filewinch`, where Filewinch has always kept its files.
    /// `None` when no home folder is set.
    pub fn default_dir() -> Option<PathBuf> {
        filewinch_os::dirs::Dirs::current().map(|d| d.config)
    }

    pub fn new(dir: impl Into<PathBuf>) -> Store {
        Store { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn load_sites(&self) -> Loaded<SitesFile> {
        load(&self.dir.join("sites.json"), "saved sites")
    }
    pub fn save_sites(&self, v: &SitesFile) -> Result<()> {
        save(&self.dir, "sites.json", v)
    }
    pub fn load_prefs(&self) -> Loaded<Prefs> {
        load(&self.dir.join("prefs.json"), "settings")
    }
    pub fn save_prefs(&self, v: &Prefs) -> Result<()> {
        save(&self.dir, "prefs.json", v)
    }
    pub fn load_recents(&self) -> Loaded<Recents> {
        load(&self.dir.join("recents.json"), "recent connections")
    }
    pub fn save_recents(&self, v: &Recents) -> Result<()> {
        save(&self.dir, "recents.json", v)
    }
}

fn load<T: DeserializeOwned + Default>(path: &Path, what: &str) -> Loaded<T> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Loaded { value: T::default(), warning: None },
        Err(e) => {
            return Loaded {
                value: T::default(),
                warning: Some(format!("Filewinch couldn't read your {what} ({e}), so it started without them.")),
            };
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(value) => Loaded { value, warning: None },
        Err(e) => {
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            let aside = path.with_file_name(format!("{name}.corrupt-{stamp}"));
            let kept = match fs::rename(path, &aside) {
                Ok(()) => format!(" The damaged file was kept as {}.", aside.file_name().unwrap_or_default().to_string_lossy()),
                Err(_) => String::new(),
            };
            Loaded {
                value: T::default(),
                warning: Some(format!("Your {what} file was damaged ({e}), so Filewinch started with an empty list.{kept}")),
            }
        }
    }
}

/// Writes `name` in `dir` atomically: a temporary file in the same folder, synced, then renamed
/// over the old one (on Windows too: `rename` replaces the file). Readable only by the owner:
/// mode 0600 on Unix; on Windows the per-user `%APPDATA%` folder's permissions already are.
fn save<T: Serialize>(dir: &Path, name: &str, v: &T) -> Result<()> {
    let fail = |e: std::io::Error| {
        let detail = format!("{}: {e}", dir.display());
        Error { message: format!("Filewinch couldn't save {name}."), ..Error::from(e) }.with_detail(detail)
    };
    fs::create_dir_all(dir).map_err(fail)?;
    let json = serde_json::to_vec_pretty(v).map_err(|e| Error::new(ErrorKind::Other, format!("Couldn't encode {name}.")).with_detail(e))?;
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let result = (|| {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut f = opts.open(&tmp)?;
        f.write_all(&json)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        fs::rename(&tmp, dir.join(name))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(fail)
}
