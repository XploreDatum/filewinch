//! Where requests go: the server origin and base path from a [`Site`], per-segment percent
//! encoding of outgoing paths, and decoding of the hrefs and `Location`s servers send back.

use filewinch_core::{Error, ErrorKind, Protocol, RPath, Result, Site};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};

/// Everything except RFC 3986 unreserved characters is encoded in a path segment, so names
/// with spaces, `#`, `%`, `?`, `+`, `;` or non-ASCII letters reach the server intact.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

#[derive(Clone, Debug)]
pub(crate) struct Target {
    pub scheme: &'static str,
    /// Host as it goes in a URL (IPv6 in brackets), lowercase.
    pub host: String,
    pub port: u16,
    /// `scheme://host[:port]`, the port left out when it's the scheme's default.
    pub origin: String,
    /// Decoded segments of the base path the site's `/` maps to.
    pub base: Vec<String>,
}

impl Target {
    /// Reads the server address from `site.host`, which may be a bare host name, `host:port`,
    /// or a full URL with a base path (`https://cloud.example.com/remote.php/dav/files/me/`).
    /// The scheme always comes from the protocol; an explicit `site.port` wins over a port in
    /// the URL, which wins over the protocol default.
    pub fn from_site(site: &Site) -> Result<Target> {
        let scheme = if site.protocol == Protocol::WebDavs { "https" } else { "http" };
        let invalid = |why: &str| {
            Error::new(ErrorKind::Other, format!("The server address “{}” isn't valid: {why}.", site.host.trim()))
        };
        let mut rest = site.host.trim();
        if rest.is_empty() {
            return Err(Error::new(ErrorKind::Other, "Enter the server address for this site."));
        }
        if let Some(i) = rest.find("://") {
            rest = &rest[i + 3..];
        }
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let path = path.split(['?', '#']).next().unwrap_or("");
        // Drop any `user@` in the address; the user name comes from the site.
        let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        let (host, url_port) = split_host_port(authority).ok_or_else(|| invalid("the port isn't a number"))?;
        if host.is_empty() || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(invalid("the host name is missing or has spaces"));
        }
        let default = if scheme == "https" { 443 } else { 80 };
        let port = site.port.or(url_port).or(site.port_or_default()).unwrap_or(default);
        let host = host.to_ascii_lowercase();
        let origin =
            if port == default { format!("{scheme}://{host}") } else { format!("{scheme}://{host}:{port}") };
        Ok(Target { scheme, host, port, origin, base: decode_segments(path) })
    }

    /// Host name for messages and the TLS trust store (no IPv6 brackets).
    pub fn host_name(&self) -> &str {
        self.host.trim_start_matches('[').trim_end_matches(']')
    }

    /// Decoded segments of `path` on the server, base path included.
    pub fn segments(&self, path: &RPath) -> Vec<String> {
        self.base.iter().cloned().chain(path.components().map(str::to_string)).collect()
    }

    /// The encoded request path for `path`; collections get a trailing slash.
    pub fn request_path(&self, path: &RPath, collection: bool) -> String {
        let mut out = String::new();
        for seg in self.segments(path) {
            out.push('/');
            out.extend(utf8_percent_encode(&seg, SEGMENT));
        }
        if collection || out.is_empty() {
            out.push('/');
        }
        out
    }

    pub fn url(&self, request_path: &str) -> String {
        format!("{}{request_path}", self.origin)
    }

    /// Resolves a redirect's `Location` against the current request path. Only the same
    /// scheme, host and port are followed; anything else is an error naming where it went.
    pub fn resolve_location(&self, location: &str, current: &str) -> Result<String> {
        let location = location.trim();
        let path = if let Some(i) = location.find("://") {
            let scheme = location[..i].to_ascii_lowercase();
            let rest = &location[i + 3..];
            let (authority, path) = rest.find('/').map_or((rest, "/"), |j| (&rest[..j], &rest[j..]));
            let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
            let (host, port) = split_host_port(authority).unwrap_or((authority, None));
            let port = port.unwrap_or(if scheme == "https" { 443 } else { 80 });
            if scheme != self.scheme || !host.eq_ignore_ascii_case(&self.host) || port != self.port {
                let hint = if scheme == "https" && self.scheme == "http" {
                    " Change the site's protocol to WebDAV (HTTPS)."
                } else {
                    ""
                };
                return Err(Error::new(
                    ErrorKind::Other,
                    format!("{} redirected Filewinch to a different address.{hint}", self.host_name()),
                )
                .with_detail(format!("Location: {scheme}://{host}:{port}")));
            }
            path.to_string()
        } else if location.starts_with('/') && !location.starts_with("//") {
            location.to_string()
        } else {
            // Relative reference: resolve against the current request's folder.
            let dir = current.rfind('/').map_or("/", |i| &current[..=i]);
            format!("{dir}{location}")
        };
        Ok(path.split('#').next().unwrap_or("/").to_string())
    }
}

/// Splits `host[:port]` (IPv6 hosts in brackets). `None` if a port is present but not a
/// number.
fn split_host_port(authority: &str) -> Option<(&str, Option<u16>)> {
    let (host, port) = if authority.starts_with('[') {
        match authority.find(']') {
            Some(i) => (&authority[..=i], authority[i + 1..].strip_prefix(':')),
            None => (authority, None),
        }
    } else if authority.matches(':').count() > 1 {
        // Unbracketed IPv6 literal: no port possible.
        return Some((authority, None));
    } else {
        match authority.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    match port {
        None | Some("") => Some((host, None)),
        Some(p) => p.parse().ok().map(|p| (host, Some(p))),
    }
}

/// Decodes the path part of an href: an absolute URL or an absolute path, percent-encoded,
/// maybe with a trailing slash. Returns the decoded, non-empty segments.
pub(crate) fn href_segments(href: &str) -> Vec<String> {
    let href = href.trim();
    let path = match href.find("://") {
        Some(i) => href[i + 3..].find('/').map_or("", |j| &href[i + 3 + j..]),
        None => href,
    };
    decode_segments(path.split(['?', '#']).next().unwrap_or(""))
}

fn decode_segments(path: &str) -> Vec<String> {
    let mut segs: Vec<String> = Vec::new();
    for raw in path.split('/') {
        let seg = percent_decode_str(raw).decode_utf8_lossy();
        match seg.as_ref() {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            s => segs.push(s.to_string()),
        }
    }
    segs
}

#[cfg(test)]
#[path = "target_tests.rs"]
mod tests;
