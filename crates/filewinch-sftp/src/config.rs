//! `~/.ssh/config` resolution for Host, HostName, User, Port, IdentityFile and IdentitiesOnly.
//!
//! Follows OpenSSH semantics: the first value obtained for a key wins; `Host` patterns support
//! `*`, `?` and `!negation`; `Include` is followed (relative to `~/.ssh`, `*` globs in the
//! file name); `Match` blocks are skipped since their conditions can't be evaluated here.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostConfig {
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_files: Vec<PathBuf>,
    pub identities_only: bool,
}

/// Resolves `alias` against `~/.ssh/config` (`%USERPROFILE%\\.ssh\\config` on Windows; a
/// missing file is an empty config).
pub fn resolve(alias: &str) -> HostConfig {
    let Some(dirs) = filewinch_os::dirs::Dirs::current() else {
        return HostConfig::default();
    };
    resolve_in(&dirs.ssh().join("config"), &dirs.home, alias)
}

pub fn resolve_in(path: &Path, home: &Path, alias: &str) -> HostConfig {
    let mut out = HostConfig::default();
    let mut identities_only = None;
    let mut raw_ids = Vec::new();
    parse_file(path, home, alias, &mut out, &mut identities_only, &mut raw_ids, 0);
    out.identities_only = identities_only.unwrap_or(false);
    let host = out.hostname.clone().unwrap_or_else(|| alias.to_string());
    let user = out.user.clone().unwrap_or_else(|| std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default());
    out.identity_files = raw_ids.iter().map(|s| PathBuf::from(expand(s, home, &host, &user))).collect();
    out
}

fn parse_file(path: &Path, home: &Path, alias: &str, out: &mut HostConfig, ids_only: &mut Option<bool>, ids: &mut Vec<String>, depth: u32) {
    if depth > 8 {
        return;
    }
    let Ok(text) = std::fs::read_to_string(path) else { return };
    let mut active = true; // lines before the first Host apply to every host
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = split_kv(line);
        let key = key.to_ascii_lowercase();
        match key.as_str() {
            "host" => active = host_matches(value, alias),
            "match" => active = false,
            _ if !active => {}
            "include" => {
                for pat in value.split_whitespace() {
                    for f in include_paths(pat, home) {
                        parse_file(&f, home, alias, out, ids_only, ids, depth + 1);
                    }
                }
            }
            "hostname" if out.hostname.is_none() => out.hostname = Some(unquote(value).to_string()),
            "user" if out.user.is_none() => out.user = Some(unquote(value).to_string()),
            "port" if out.port.is_none() => out.port = unquote(value).parse().ok(),
            "identityfile" => ids.push(unquote(value).to_string()),
            "identitiesonly" if ids_only.is_none() => *ids_only = Some(value.eq_ignore_ascii_case("yes")),
            _ => {}
        }
    }
}

fn split_kv(line: &str) -> (&str, &str) {
    let i = line.find(|c: char| c.is_whitespace() || c == '=').unwrap_or(line.len());
    let key = &line[..i];
    let rest = line[i..].trim_start_matches(|c: char| c.is_whitespace() || c == '=');
    (key, rest.trim())
}

fn unquote(s: &str) -> &str {
    s.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(s)
}

fn host_matches(patterns: &str, alias: &str) -> bool {
    let mut matched = false;
    for p in patterns.split_whitespace() {
        if let Some(neg) = p.strip_prefix('!') {
            if wildcard(neg, alias) {
                return false;
            }
        } else if wildcard(p, alias) {
            matched = true;
        }
    }
    matched
}

/// `*` and `?` glob, case-insensitive (host names are).
pub(crate) fn wildcard(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let t: Vec<char> = text.to_ascii_lowercase().chars().collect();
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

fn include_paths(pat: &str, home: &Path) -> Vec<PathBuf> {
    let pat = if let Some(rest) = pat.strip_prefix("~/").or_else(|| pat.strip_prefix("~\\")) {
        home.join(rest)
    } else if Path::new(pat).is_absolute() || pat.starts_with('/') {
        PathBuf::from(pat)
    } else {
        home.join(".ssh").join(pat)
    };
    let name = pat.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !name.contains(['*', '?']) {
        return vec![pat];
    }
    let Some(dir) = pat.parent() else { return vec![] };
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| wildcard(&name, &e.file_name().to_string_lossy()))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}

/// Expands `~`, `%d` (home), `%h` (host), `%r`/`%u` (user) and `%%`.
fn expand(s: &str, home: &Path, host: &str, user: &str) -> String {
    let s = match s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        Some(rest) => format!("{}/{rest}", home.display()),
        None => s.to_string(),
    };
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('d') => out.push_str(&home.display().to_string()),
            Some('h') => out.push_str(host),
            Some('r') | Some('u') => out.push_str(user),
            Some('%') => out.push('%'),
            Some(o) => {
                out.push('%');
                out.push(o);
            }
            None => out.push('%'),
        }
    }
    out
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
