//! Hosts from `~/.ssh/config` (`%USERPROFILE%\.ssh\config` on Windows), offered as SFTP
//! suggestions.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SshHost {
    /// The `Host` alias.
    pub alias: String,
    /// `HostName`, else the alias.
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<String>,
}

/// Reads `~/.ssh/config` and the files it `Include`s. A missing file gives no hosts.
pub fn read_ssh_config() -> Vec<SshHost> {
    let Some(dirs) = filewinch_os::dirs::Dirs::current() else { return Vec::new() };
    read_ssh_config_in(&dirs.home, &dirs.ssh())
}

/// `<ssh>/config` with `~` meaning `home` and relative `Include`s under `ssh`.
pub(crate) fn read_ssh_config_in(home: &Path, ssh: &Path) -> Vec<SshHost> {
    let mut out = Vec::new();
    read_into(&ssh.join("config"), home, ssh, &mut out, 0);
    out
}

fn read_into(path: &Path, home: &Path, ssh: &Path, out: &mut Vec<SshHost>, depth: u32) {
    let Ok(src) = std::fs::read_to_string(path) else { return };
    let includes = parse_into(&src, home, out);
    if depth >= 8 {
        return; // include loops
    }
    for pattern in includes {
        for file in glob(&pattern, home, ssh) {
            read_into(&file, home, ssh, out, depth + 1);
        }
    }
}

/// Parses config text into hosts. Wildcard-only `Host` patterns (`*`, `*.example.com`) and
/// `Match` blocks aren't hosts one can connect to, so they're skipped.
pub fn parse_ssh_config(src: &str) -> Vec<SshHost> {
    let mut out = Vec::new();
    parse_into(src, Path::new("/"), &mut out);
    out
}

/// Returns the `Include` patterns found.
fn parse_into(src: &str, home: &Path, out: &mut Vec<SshHost>) -> Vec<String> {
    let mut includes = Vec::new();
    // Indexes into `out` of the hosts the current block applies to.
    let mut current: Vec<usize> = Vec::new();
    for line in src.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = match line.split_once(|c: char| c.is_whitespace() || c == '=') {
            Some((k, v)) => (k.to_ascii_lowercase(), unquote(v.trim_start_matches(|c: char| c.is_whitespace() || c == '=').trim())),
            None => continue,
        };
        match key.as_str() {
            "host" => {
                current.clear();
                for alias in value.split_whitespace() {
                    if alias.contains(['*', '?', '!']) {
                        continue;
                    }
                    if let Some(i) = out.iter().position(|h: &SshHost| h.alias == alias) {
                        current.push(i);
                    } else {
                        out.push(SshHost { alias: alias.to_owned(), host: alias.to_owned(), ..SshHost::default() });
                        current.push(out.len() - 1);
                    }
                }
            }
            "match" => current.clear(),
            "include" => includes.extend(value.split_whitespace().map(str::to_owned)),
            // ssh takes the first value given for each keyword.
            "hostname" => each(out, &current, |h| if h.host == h.alias { h.host = value.clone() }),
            "user" => each(out, &current, |h| { h.user.get_or_insert_with(|| value.clone()); }),
            "port" => each(out, &current, |h| { if h.port.is_none() { h.port = value.parse().ok() } }),
            "identityfile" => {
                let path = expand(&value, home);
                each(out, &current, |h| { h.identity_file.get_or_insert_with(|| path.clone()); })
            }
            _ => {}
        }
    }
    includes
}

fn each(out: &mut [SshHost], idx: &[usize], mut f: impl FnMut(&mut SshHost)) {
    for &i in idx {
        f(&mut out[i]);
    }
}

fn unquote(s: &str) -> String {
    s.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(s).to_owned()
}

/// A leading `~/` (also `~\` on Windows) is the home folder. Other text, Windows paths with
/// backslashes and drive letters included, is kept as written.
fn expand(p: &str, home: &Path) -> String {
    let rest = p.strip_prefix("~/").or_else(|| if cfg!(windows) { p.strip_prefix("~\\") } else { None });
    match rest {
        Some(rest) => home.join(rest).to_string_lossy().into_owned(),
        None => p.to_owned(),
    }
}

/// Files matching an `Include` pattern: relative patterns are under the `.ssh` folder; `*`
/// and `?` match within the last component.
fn glob(pattern: &str, home: &Path, ssh: &Path) -> Vec<PathBuf> {
    let full = PathBuf::from(expand(pattern, home));
    let full = if full.is_absolute() { full } else { ssh.join(full) };
    let name = full.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !name.contains(['*', '?']) {
        return vec![full];
    }
    let Some(dir) = full.parent() else { return Vec::new() };
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<PathBuf> =
        rd.flatten().filter(|e| wild_match(&name, &e.file_name().to_string_lossy())).map(|e| e.path()).collect();
    files.sort();
    files
}

fn wild_match(p: &str, s: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (p.chars().collect(), s.chars().collect());
    fn go(p: &[char], s: &[char]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some('*'), _) => go(&p[1..], s) || (!s.is_empty() && go(p, &s[1..])),
            (Some('?'), Some(_)) => go(&p[1..], &s[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &s[1..]),
            _ => false,
        }
    }
    go(&p, &s)
}

#[cfg(test)]
pub(crate) fn wild(p: &str, s: &str) -> bool {
    wild_match(p, s)
}
