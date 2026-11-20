//! Where Filewinch keeps its files on each OS:
//!
//! | | macOS | Windows | Linux (XDG) |
//! | --- | --- | --- | --- |
//! | config | `~/Library/Application Support/Filewinch` | `%APPDATA%\Filewinch` | `$XDG_CONFIG_HOME/filewinch` (`~/.config`) |
//! | cache | `~/Library/Caches/app.xpdt.filewinch` | `%LOCALAPPDATA%\Filewinch\Cache` | `$XDG_CACHE_HOME/filewinch` (`~/.cache`) |
//! | ssh | `~/.ssh` | `%USERPROFILE%\.ssh` | `~/.ssh` |
//!
//! Config holds settings, Favorites, recents, `known_hosts`, trusted certificates and the
//! OAuth clients file; the cache holds drag-out staging and edit-in-place copies. The macOS
//! paths are the ones Filewinch has always used, so nothing moves on upgrade.
//!
//! Home is `HOME`, or on Windows `USERPROFILE` (then `HOMEDRIVE` + `HOMEPATH`). Windows falls
//! back to `<home>\AppData\Roaming` / `Local` when `APPDATA` / `LOCALAPPDATA` is unset, and a
//! relative XDG variable is ignored, as the XDG spec says. [`ROOT_ENV`] moves config and cache
//! under one folder for tests and isolated runs.
//!
//! Everything resolves from an injected lookup ([`Dirs::from_env`]), so each OS's rules are
//! tested on any OS. Nothing here touches the disk.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::Os;

/// `FILEWINCH_DIRS_ROOT=<dir>` puts config in `<dir>/config` and the cache in `<dir>/cache`.
pub const ROOT_ENV: &str = "FILEWINCH_DIRS_ROOT";

/// Filewinch's folders on one OS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dirs {
    /// Settings, Favorites, recents, known hosts, trusted certificates, OAuth clients.
    pub config: PathBuf,
    /// Files Filewinch can rebuild: drag-out staging, edit-in-place copies.
    pub cache: PathBuf,
    /// The person's home folder.
    pub home: PathBuf,
}

impl Dirs {
    /// This OS's folders from the process environment. `None` when no home folder is set.
    pub fn current() -> Option<Dirs> {
        Dirs::from_env(Os::CURRENT, |k| std::env::var_os(k))
    }

    /// `os`'s folders with the environment looked up through `var`.
    pub fn from_env(os: Os, var: impl Fn(&str) -> Option<OsString>) -> Option<Dirs> {
        let set = |k: &str| var(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let home = home_from(os, &var)?;
        if let Some(root) = set(ROOT_ENV) {
            return Some(Dirs { config: root.join("config"), cache: root.join("cache"), home });
        }
        Some(match os {
            Os::Mac => Dirs {
                config: home.join("Library").join("Application Support").join("Filewinch"),
                cache: home.join("Library").join("Caches").join(crate::APP_ID),
                home,
            },
            Os::Windows => {
                let app_data = |k: &str, leaf: &str| set(k).unwrap_or_else(|| home.join("AppData").join(leaf));
                Dirs {
                    config: app_data("APPDATA", "Roaming").join("Filewinch"),
                    cache: app_data("LOCALAPPDATA", "Local").join("Filewinch").join("Cache"),
                    home,
                }
            }
            Os::Linux => {
                let xdg = |k: &str, fallback: &str| set(k).filter(|p| p.is_absolute()).unwrap_or_else(|| home.join(fallback));
                Dirs { config: xdg("XDG_CONFIG_HOME", ".config").join("filewinch"), cache: xdg("XDG_CACHE_HOME", ".cache").join("filewinch"), home }
            }
        })
    }

    /// `~/.ssh` (`%USERPROFILE%\.ssh` on Windows), where OpenSSH keeps config and keys.
    pub fn ssh(&self) -> PathBuf {
        self.home.join(".ssh")
    }
}

/// The home folder of `os` from `var`: `HOME`, or on Windows `USERPROFILE`, then
/// `HOMEDRIVE` + `HOMEPATH`, then `HOME` (set by MSYS and Git Bash).
pub fn home_from(os: Os, var: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let set = |k: &str| var(k).filter(|v| !v.is_empty());
    let home = match os {
        Os::Windows => set("USERPROFILE")
            .or_else(|| {
                let mut drive = set("HOMEDRIVE")?;
                drive.push(set("HOMEPATH")?);
                Some(drive)
            })
            .or_else(|| set("HOME")),
        Os::Mac | Os::Linux => set("HOME"),
    };
    home.map(PathBuf::from)
}

/// This OS's home folder from the process environment.
pub fn home() -> Option<PathBuf> {
    home_from(Os::CURRENT, |k| std::env::var_os(k))
}

/// Expands a leading `~/` (or `~\` on Windows) to the home folder; anything else is as given.
pub fn expand_tilde(p: &str) -> PathBuf {
    let rest = p.strip_prefix("~/").or_else(|| if Os::CURRENT == Os::Windows { p.strip_prefix("~\\") } else { None });
    match (rest, home()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ if p == "~" => home().unwrap_or_else(|| PathBuf::from(p)),
        _ => PathBuf::from(p),
    }
}
