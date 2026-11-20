//! Turning a saved [`Site`] into an endpoint, an account and a credential. The rules are
//! the ones documented in the crate docs; keep the two in step.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use filewinch_core::{Auth, Error, ErrorKind, RPath, Result, SecretRef, Site};
use percent_encoding::percent_decode_str;

/// How requests are authorized. No `Debug`: it holds the key or token.
pub(crate) enum Credential {
    /// Decoded account key.
    SharedKey(Vec<u8>),
    /// SAS query string, without the leading `?` (already percent-encoded).
    Sas(String),
    /// No credential: public containers only.
    Anonymous,
}

pub(crate) struct Config {
    /// `https://acct.blob.core.windows.net` or `http://127.0.0.1:10000`, no trailing slash.
    pub origin: String,
    /// Host name for messages (never a URL, never a token).
    pub host: String,
    /// `""` for virtual-hosted endpoints, `/account` for path-style ones (Azurite, IPs).
    pub path_prefix: String,
    pub account: String,
    pub credential: Credential,
    /// The only container this credential can reach (container SAS, or anonymous access).
    pub scoped: Option<String>,
    pub home: RPath,
}

pub(crate) const DEFAULT_SUFFIX: &str = "blob.core.windows.net";

pub(crate) fn parse(
    site: &Site,
    secret: &dyn Fn(&SecretRef) -> Result<Option<String>>,
) -> Result<Config> {
    let raw = site.host.trim();
    if raw.is_empty() {
        return Err(invalid("Enter the storage account name or the Blob service URL."));
    }
    let (scheme, authority, url_path, url_query) = split_url(raw)?;
    let (host_only, url_port) = split_port(&authority)?;
    if host_only.is_empty() {
        return Err(invalid("The Blob service address has no host name."));
    }
    let port = site.port.or(url_port);

    // Bare account name: the public Azure endpoint.
    let bare_account = !raw.contains("://") && !host_only.contains('.') && !is_ip_or_localhost(&host_only);
    let host = if bare_account { format!("{}.{DEFAULT_SUFFIX}", host_only.to_ascii_lowercase()) } else { host_only };
    let origin = match port {
        Some(p) => format!("{scheme}://{host}:{p}"),
        None => format!("{scheme}://{host}"),
    };

    let mut segments: Vec<String> = url_path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned())
        .collect();
    let path_style = site.path_style || is_ip_or_localhost(&host);
    let user = site.user.trim();
    let account = if path_style {
        if segments.is_empty() { user.to_string() } else { segments.remove(0) }
    } else if !user.is_empty() {
        user.to_string()
    } else {
        host.split('.').next().unwrap_or_default().to_string()
    };
    if account.is_empty() {
        return Err(invalid(
            "Enter the storage account name (in the account field, or in the address as http://host:port/account).",
        ));
    }
    let path_prefix = if path_style { format!("/{account}") } else { String::new() };
    let url_location = segments.join("/");

    let credential = credential(site, secret, &account, url_query)?;
    let sas_scope = match &credential {
        Credential::Sas(q) => sas_resource(q),
        _ => None,
    };
    let initial = site.initial_path.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let location = initial.map(str::to_string).unwrap_or(url_location.clone());
    let first = |s: &str| RPath::new(s).components().next().map(str::to_string);

    let scoped = match (&credential, sas_scope.as_deref()) {
        (Credential::Sas(_), Some("c")) => match first(&url_location).or_else(|| first(&location)) {
            Some(c) => Some(c),
            None => {
                return Err(invalid(
                    "This SAS token is for one container. Paste the full container SAS URL, or put the container name in the initial folder.",
                ));
            }
        },
        (Credential::Sas(_), Some("b" | "bs" | "bv")) => {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "A SAS token for a single blob can't be used to browse. Use an account SAS or a container SAS.",
            ));
        }
        (Credential::Anonymous, _) => match first(&location) {
            Some(c) => Some(c),
            None => {
                return Err(Error::new(
                    ErrorKind::Auth,
                    format!(
                        "Enter the account key or a SAS token for {account}. Without one, only a public container can be opened; put its name in the initial folder."
                    ),
                ));
            }
        },
        _ => None,
    };

    let mut home = RPath::new(&location);
    if let Some(c) = &scoped
        && !home.starts_with(&RPath::root().join(c)) {
            home = RPath::root().join(c);
        }
    Ok(Config { origin, host, path_prefix, account, credential, scoped, home })
}

fn credential(
    site: &Site,
    secret: &dyn Fn(&SecretRef) -> Result<Option<String>>,
    account: &str,
    url_query: Option<String>,
) -> Result<Credential> {
    if let Some(q) = url_query.filter(|q| !q.is_empty()) {
        return sas(&q);
    }
    match &site.auth {
        Auth::None => Ok(Credential::Anonymous),
        Auth::Password { secret: r } => {
            let value = secret(r)?.unwrap_or_default();
            let value = value.trim();
            if value.is_empty() {
                return Err(Error::new(
                    ErrorKind::Auth,
                    format!("No account key or SAS token is saved for {account}."),
                ));
            }
            if looks_like_sas(value) {
                return sas(value);
            }
            let compact: String = value.split_whitespace().collect();
            match STANDARD.decode(compact.as_bytes()) {
                Ok(k) if !k.is_empty() => Ok(Credential::SharedKey(k)),
                _ => Err(Error::new(
                    ErrorKind::Auth,
                    format!(
                        "The account key for {account} isn't valid. Copy key1 or key2 from the storage account's Access keys page in the Azure portal."
                    ),
                )),
            }
        }
        _ => Err(Error::new(
            ErrorKind::Unsupported,
            "Azure Blob Storage signs in with an account key or a SAS token.",
        )),
    }
}

/// An account key is plain base64, which never contains `?`, `&` or `sig=` mid-string.
pub(crate) fn looks_like_sas(value: &str) -> bool {
    value.starts_with('?') || value.contains('&') || value.starts_with("sig=")
}

fn sas(value: &str) -> Result<Credential> {
    let q = value.trim().trim_start_matches('?').trim();
    let has_sig = q.split('&').any(|p| p.split_once('=').is_some_and(|(k, v)| k == "sig" && !v.is_empty()));
    if !has_sig {
        return Err(Error::new(
            ErrorKind::Auth,
            "The SAS token is incomplete: it has no signature (sig=). Copy the whole token or SAS URL again.",
        ));
    }
    Ok(Credential::Sas(q.to_string()))
}

/// The `sr` (signed resource) of a service SAS: `c` container, `b` blob. None for an
/// account SAS.
fn sas_resource(q: &str) -> Option<String> {
    q.split('&').find_map(|p| p.strip_prefix("sr=")).map(|v| v.to_ascii_lowercase())
}

/// Splits `scheme://authority/path?query`. A value without a scheme is a host name.
fn split_url(raw: &str) -> Result<(String, String, String, Option<String>)> {
    let (scheme, rest) = match raw.split_once("://") {
        Some((s, r)) => {
            let s = s.to_ascii_lowercase();
            if s != "http" && s != "https" {
                return Err(invalid("The Blob service address must start with https:// or http://."));
            }
            (s, r)
        }
        None => ("https".to_string(), raw),
    };
    let (rest, query) = match rest.split_once('?') {
        Some((r, q)) => (r, Some(q.to_string())),
        None => (rest, None),
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if authority.contains('@') {
        return Err(invalid("Put the account key in the password field, not in the address."));
    }
    Ok((scheme, authority.to_string(), path.to_string(), query))
}

fn split_port(authority: &str) -> Result<(String, Option<u16>)> {
    // IPv6 literals: [::1]:10000
    let (host, port) = if let Some(end) = authority.strip_prefix('[').and_then(|r| r.find(']')) {
        let host = &authority[..end + 2];
        (host, authority[end + 2..].strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    let port = match port {
        Some(p) => Some(p.parse::<u16>().map_err(|_| invalid("The port in the Blob service address isn't a number."))?),
        None => None,
    };
    Ok((host.to_ascii_lowercase(), port))
}

fn is_ip_or_localhost(host: &str) -> bool {
    host == "localhost"
        || host.starts_with('[')
        || host.parse::<std::net::Ipv4Addr>().is_ok()
}

fn invalid(message: &str) -> Error {
    Error::new(ErrorKind::InvalidPath, message)
}
