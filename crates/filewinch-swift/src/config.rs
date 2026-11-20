//! Reading a [`Site`] into what signing in to Swift needs. The rules are in the crate docs.

use filewinch_core::{Error, ErrorKind, Result, Site};
use filewinch_http::ureq::http::Uri;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Version {
    V1,
    V2,
    V3,
    /// No version in the URL: try v3, then v2, then v1.
    Detect,
}

/// Everything needed to sign in. No `Debug`: it holds the password.
pub(crate) struct AuthConfig {
    /// Auth URL without a trailing slash (and, for `Detect`, without a version).
    pub base: String,
    pub version: Version,
    /// The user field as typed (sent as-is for v1, shown in messages).
    pub user: String,
    pub user_name: String,
    pub project: Option<String>,
    pub domain: String,
    pub secret: String,
    pub region: Option<String>,
    /// The auth server's host name, for messages.
    pub host: String,
    /// The site's "always trust" certificate fingerprints.
    pub trusted: Vec<String>,
}

impl AuthConfig {
    pub fn from_site(site: &Site, secret: String) -> Result<AuthConfig> {
        let (base, version, host) = parse_auth_url(&site.host, site.port)?;
        let user_name = site.user.trim().to_string();
        if user_name.is_empty() {
            return Err(Error::new(ErrorKind::Auth, "Enter a user name for this Swift account."));
        }
        let clean = |v: &Option<String>| v.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
        let project = clean(&site.project);
        // TempAuth (v1) takes `account:user` in one header; the project is its account.
        let user = match &project {
            Some(p) => format!("{p}:{user_name}"),
            None => user_name.clone(),
        };
        Ok(AuthConfig {
            base,
            version,
            user_name,
            project,
            domain: clean(&site.domain).unwrap_or_else(|| "Default".into()),
            user,
            trusted: site.trusted_fingerprints.clone(),
            secret,
            region: site.region.as_deref().map(str::trim).filter(|r| !r.is_empty()).map(str::to_string),
            host,
        })
    }

    /// True when the v2 request should use Rackspace's `RAX-KSKEY:apiKeyCredentials`.
    pub fn use_api_key(&self) -> bool {
        uses_rackspace_api_key(&self.host, &self.secret)
    }
}

pub(crate) fn uses_rackspace_api_key(host: &str, secret: &str) -> bool {
    let host = host.to_ascii_lowercase();
    (host == "rackspacecloud.com" || host.ends_with(".rackspacecloud.com"))
        && secret.len() == 32
        && secret.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Returns (base URL, version, host name).
pub(crate) fn parse_auth_url(raw: &str, port: Option<u16>) -> Result<(String, Version, String)> {
    let bad = || {
        Error::new(
            ErrorKind::InvalidPath,
            "The auth URL isn't valid. Enter it like https://keystone.example.com:5000/v3.",
        )
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(bad());
    }
    let with_scheme = if raw.contains("://") { raw.to_string() } else { format!("https://{raw}") };
    let uri: Uri = with_scheme.parse().map_err(|_| bad())?;
    let scheme = uri.scheme_str().ok_or_else(bad)?.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(bad());
    }
    let authority = uri.authority().ok_or_else(bad)?;
    let host = authority.host().trim_matches(|c| c == '[' || c == ']').to_string();
    if host.is_empty() {
        return Err(bad());
    }
    let authority = match (authority.port_u16(), port) {
        (None, Some(p)) => format!("{}:{p}", authority.as_str()),
        _ => authority.as_str().to_string(),
    };
    let mut path = uri.path().trim_end_matches('/').to_string();
    let lower = path.to_ascii_lowercase();
    // A pasted token endpoint: keep the versioned base.
    for suffix in ["/auth/tokens", "/tokens"] {
        if lower.ends_with(suffix) && (lower.contains("/v3") || lower.contains("/v2")) {
            path.truncate(path.len() - suffix.len());
            break;
        }
    }
    let lower = path.to_ascii_lowercase();
    let version = if lower.ends_with("/v3") {
        Version::V3
    } else if lower.ends_with("/v2.0") || lower.ends_with("/v2") {
        Version::V2
    } else if lower.ends_with("/v1.0") || lower.ends_with("/v1") {
        Version::V1
    } else {
        Version::Detect
    };
    Ok((format!("{scheme}://{authority}{path}"), version, host))
}
