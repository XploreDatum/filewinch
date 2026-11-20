/// Every protocol Filewinch speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Protocol {
    Local,
    Sftp,
    Ftp,
    /// FTP with TLS; see [`TlsMode`].
    Ftps,
    WebDav,
    /// WebDAV over HTTPS.
    WebDavs,
    /// Amazon S3 and S3-compatible endpoints (R2, MinIO, Wasabi, DigitalOcean Spaces, B2's S3 API).
    S3,
    /// Backblaze B2 native API.
    B2,
    AzureBlob,
    /// OpenStack Swift, including Rackspace Cloud Files.
    Swift,
    GoogleDrive,
    Dropbox,
    OneDrive,
    Box,
}

impl Protocol {
    pub fn label(self) -> &'static str {
        match self {
            Protocol::Local => "This Mac",
            Protocol::Sftp => "SFTP",
            Protocol::Ftp => "FTP",
            Protocol::Ftps => "FTP with TLS",
            Protocol::WebDav => "WebDAV",
            Protocol::WebDavs => "WebDAV (HTTPS)",
            Protocol::S3 => "Amazon S3",
            Protocol::B2 => "Backblaze B2",
            Protocol::AzureBlob => "Microsoft Azure",
            Protocol::Swift => "OpenStack Swift",
            Protocol::GoogleDrive => "Google Drive",
            Protocol::Dropbox => "Dropbox",
            Protocol::OneDrive => "Microsoft OneDrive",
            Protocol::Box => "Box",
        }
    }

    pub fn default_port(self) -> Option<u16> {
        match self {
            Protocol::Sftp => Some(22),
            Protocol::Ftp | Protocol::Ftps => Some(21),
            Protocol::WebDav => Some(80),
            Protocol::WebDavs => Some(443),
            _ => None,
        }
    }

    /// Signs in through the browser (OAuth) instead of a typed password.
    pub fn uses_oauth(self) -> bool {
        matches!(self, Protocol::GoogleDrive | Protocol::Dropbox | Protocol::OneDrive | Protocol::Box)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum TlsMode {
    /// FTPS: `AUTH TLS` on port 21.
    #[default]
    Explicit,
    /// FTPS: TLS from the first byte, usually port 990.
    Implicit,
}

/// Points at a secret in the macOS Keychain (service `app.xpdt.filewinch`, account = this
/// string). Secrets never live in the site file.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SecretRef(pub String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Auth {
    None,
    /// Password (or S3 secret key, Azure account key, Swift API key, B2 application key).
    Password { secret: SecretRef },
    /// SSH private key file, with its passphrase in the Keychain if it has one.
    KeyFile { path: String, passphrase: Option<SecretRef> },
    /// Use the running ssh-agent.
    Agent,
    /// OAuth refresh token stored in the Keychain.
    OAuth { refresh_token: SecretRef },
}

/// A saved connection (a Favorite). The UI stores these; backends read only what they need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Site {
    pub id: String,
    pub name: String,
    pub protocol: Protocol,
    /// Hostname, or endpoint URL for S3-compatible / Swift auth URL / Azure account name.
    pub host: String,
    pub port: Option<u16>,
    /// Username, S3 access key ID, B2 key ID, Azure account, Swift user.
    pub user: String,
    pub auth: Auth,
    /// Folder (or bucket/container, or `bucket/prefix`) to open in.
    pub initial_path: Option<String>,
    /// S3 / Swift region.
    pub region: Option<String>,
    pub tls: TlsMode,
    /// FTP passive mode (default true).
    pub passive: bool,
    /// S3: path-style addressing for MinIO and similar.
    pub path_style: bool,
    /// Matching local folder for "sync" and the local pane when this site opens.
    pub local_path: Option<String>,
    /// Host-key / TLS certificate fingerprints the user chose to always trust for this site
    /// (canonical `SHA256:<base64>`; see [`crate::normalize_fingerprint`], which also accepts
    /// the hex `sha256:AB:CD:…` form). Backends accept these on top of the system trust store
    /// and their own known-hosts files.
    pub trusted_fingerprints: Vec<String>,
    /// OpenStack Swift (Keystone): project (v3) / tenant (v2) name to scope the token to.
    ///
    /// Typed fields rather than a free-form `options` map, like `region`/`path_style`/`passive`:
    /// the connect sheet labels and validates each one, the store serialises them by name, and a
    /// typo can't silently become an ignored key.
    pub project: Option<String>,
    /// OpenStack Swift (Keystone v3): domain of the user and project; `None` means "Default".
    pub domain: Option<String>,
}

impl Site {
    pub fn new(id: impl Into<String>, name: impl Into<String>, protocol: Protocol) -> Self {
        Site {
            id: id.into(),
            name: name.into(),
            protocol,
            host: String::new(),
            port: None,
            user: String::new(),
            auth: Auth::None,
            initial_path: None,
            region: None,
            tls: TlsMode::default(),
            passive: true,
            path_style: false,
            local_path: None,
            trusted_fingerprints: Vec::new(),
            project: None,
            domain: None,
        }
    }

    pub fn port_or_default(&self) -> Option<u16> {
        self.port.or(self.protocol.default_port())
    }
}
