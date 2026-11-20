//! JSON form of [`Site`] (core types carry no serde): protocols and modes as stable lowercase
//! ids, secrets only as Keychain account names.

use filewinch_core::{Auth, Protocol, SecretRef, Site, TlsMode};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const PROTOCOL_IDS: [(Protocol, &str); 14] = [
    (Protocol::Local, "local"),
    (Protocol::Sftp, "sftp"),
    (Protocol::Ftp, "ftp"),
    (Protocol::Ftps, "ftps"),
    (Protocol::WebDav, "webdav"),
    (Protocol::WebDavs, "webdavs"),
    (Protocol::S3, "s3"),
    (Protocol::B2, "b2"),
    (Protocol::AzureBlob, "azure"),
    (Protocol::Swift, "swift"),
    (Protocol::GoogleDrive, "gdrive"),
    (Protocol::Dropbox, "dropbox"),
    (Protocol::OneDrive, "onedrive"),
    (Protocol::Box, "box"),
];

pub fn protocol_id(p: Protocol) -> &'static str {
    PROTOCOL_IDS.iter().find(|(q, _)| *q == p).map_or("local", |(_, id)| id)
}

pub fn protocol_from_id(id: &str) -> Option<Protocol> {
    PROTOCOL_IDS.iter().find(|(_, s)| *s == id).map(|(p, _)| *p)
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AuthRecord {
    None,
    Password { secret: String },
    KeyFile { path: String, #[serde(default)] passphrase: Option<String> },
    Agent,
    #[serde(rename = "oauth")]
    OAuth { refresh_token: String },
}

#[derive(Serialize, Deserialize)]
struct SiteRecord {
    id: String,
    name: String,
    protocol: String,
    #[serde(default)]
    host: String,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    user: String,
    auth: AuthRecord,
    #[serde(default)]
    initial_path: Option<String>,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    tls_implicit: bool,
    #[serde(default = "yes")]
    passive: bool,
    #[serde(default)]
    path_style: bool,
    #[serde(default)]
    local_path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    trusted_fingerprints: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    domain: Option<String>,
}

fn yes() -> bool {
    true
}

pub fn serialize<S: Serializer>(site: &Site, s: S) -> Result<S::Ok, S::Error> {
    let auth = match &site.auth {
        Auth::None => AuthRecord::None,
        Auth::Password { secret } => AuthRecord::Password { secret: secret.0.clone() },
        Auth::KeyFile { path, passphrase } => {
            AuthRecord::KeyFile { path: path.clone(), passphrase: passphrase.as_ref().map(|r| r.0.clone()) }
        }
        Auth::Agent => AuthRecord::Agent,
        Auth::OAuth { refresh_token } => AuthRecord::OAuth { refresh_token: refresh_token.0.clone() },
    };
    SiteRecord {
        id: site.id.clone(),
        name: site.name.clone(),
        protocol: protocol_id(site.protocol).to_owned(),
        host: site.host.clone(),
        port: site.port,
        user: site.user.clone(),
        auth,
        initial_path: site.initial_path.clone(),
        region: site.region.clone(),
        tls_implicit: site.tls == TlsMode::Implicit,
        passive: site.passive,
        path_style: site.path_style,
        local_path: site.local_path.clone(),
        trusted_fingerprints: site.trusted_fingerprints.clone(),
        project: site.project.clone(),
        domain: site.domain.clone(),
    }
    .serialize(s)
}

pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Site, D::Error> {
    let r = SiteRecord::deserialize(d)?;
    let protocol = protocol_from_id(&r.protocol)
        .ok_or_else(|| serde::de::Error::custom(format!("unknown protocol {:?}", r.protocol)))?;
    let auth = match r.auth {
        AuthRecord::None => Auth::None,
        AuthRecord::Password { secret } => Auth::Password { secret: SecretRef(secret) },
        AuthRecord::KeyFile { path, passphrase } => Auth::KeyFile { path, passphrase: passphrase.map(SecretRef) },
        AuthRecord::Agent => Auth::Agent,
        AuthRecord::OAuth { refresh_token } => Auth::OAuth { refresh_token: SecretRef(refresh_token) },
    };
    let (mut user, mut project, mut domain) = (r.user, r.project, r.domain);
    // Earlier builds packed Swift's scope into the user as `[domain/]project:user`.
    if protocol == Protocol::Swift && project.is_none()
        && let Some((scope, name)) = user.split_once(':').map(|(s, n)| (s.to_owned(), n.to_owned()))
    {
        let (d, p) = scope.split_once('/').map_or((None, scope.as_str()), |(d, p)| (Some(d), p));
        domain = domain.or(d.filter(|d| !d.is_empty()).map(str::to_owned));
        project = Some(p.to_owned());
        user = name;
    }
    Ok(Site {
        id: r.id,
        name: r.name,
        protocol,
        host: r.host,
        port: r.port,
        user,
        auth,
        initial_path: r.initial_path,
        region: r.region,
        tls: if r.tls_implicit { TlsMode::Implicit } else { TlsMode::Explicit },
        passive: r.passive,
        path_style: r.path_style,
        local_path: r.local_path,
        trusted_fingerprints: normalized_fingerprints(r.trusted_fingerprints),
        project: project.filter(|p| !p.trim().is_empty()),
        domain: domain.filter(|d| !d.trim().is_empty()),
    })
}

/// Canonical form (`SHA256:<base64>`), deduplicated; a value we can't parse is kept as written
/// so nothing the person trusted is silently dropped.
pub(crate) fn normalized_fingerprints(fps: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for fp in fps {
        let fp = filewinch_core::normalize_fingerprint(&fp).unwrap_or(fp);
        if !out.contains(&fp) {
            out.push(fp);
        }
    }
    out
}
