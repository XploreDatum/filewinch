//! The connect sheet's fields, per protocol, and their validation.

use std::collections::BTreeMap;

use filewinch_core::{Auth, Protocol, SecretRef, Site, TlsMode};
use filewinch_store::{Favorite, SshHost, new_id, secret_account};
use rune::TextEdit;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthChoice {
    Password,
    KeyFile,
    Agent,
}

/// A field the sheet shows; also the key its inline error is filed under.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Field {
    Name,
    Host,
    Port,
    User,
    Password,
    KeyPath,
    Region,
    /// OpenStack Swift: project (Keystone v3) / tenant (v2).
    Project,
    /// OpenStack Swift (Keystone v3): domain of the user and project.
    Domain,
    Path,
    SignIn,
}

pub struct Form {
    /// The favorite being edited, if any.
    pub editing: Option<String>,
    pub protocol: Protocol,
    pub name: TextEdit,
    pub host: TextEdit,
    pub port: TextEdit,
    pub user: TextEdit,
    pub password: TextEdit,
    pub auth: AuthChoice,
    pub key_path: TextEdit,
    pub passphrase: TextEdit,
    pub path: TextEdit,
    pub region: TextEdit,
    pub project: TextEdit,
    pub domain: TextEdit,
    pub tls: TlsMode,
    pub passive: bool,
    pub path_style: bool,
    pub save: bool,
    pub folder: Option<String>,
    /// An OAuth refresh token from "Sign in with …", held until the site is saved.
    pub oauth_token: Option<String>,
    pub oauth_account: Option<String>,
    pub signing_in: bool,
    /// The Keychain already holds this favorite's password: leaving the field empty keeps it.
    pub has_saved_secret: bool,
    pub errors: BTreeMap<Field, String>,
    pub picker_open: bool,
    pub suggestions: Vec<SshHost>,
    /// Trusted certificate/host-key fingerprints carried over when editing.
    pub trusted: Vec<String>,
    /// The pane the connection opens in; `None` picks an empty pane, else the active one.
    pub pane: Option<usize>,
}

impl Form {
    pub fn new(protocol: Protocol) -> Form {
        Form {
            editing: None,
            protocol,
            name: TextEdit::new(""),
            host: TextEdit::new(""),
            port: TextEdit::new(""),
            user: TextEdit::new(""),
            password: TextEdit::new(""),
            auth: AuthChoice::Password,
            key_path: TextEdit::new(""),
            passphrase: TextEdit::new(""),
            path: TextEdit::new(""),
            region: TextEdit::new(""),
            project: TextEdit::new(""),
            domain: TextEdit::new(""),
            tls: TlsMode::Explicit,
            passive: true,
            path_style: false,
            save: true,
            folder: None,
            oauth_token: None,
            oauth_account: None,
            signing_in: false,
            has_saved_secret: false,
            errors: BTreeMap::new(),
            picker_open: false,
            suggestions: Vec::new(),
            trusted: Vec::new(),
            pane: None,
        }
    }

    /// The form for editing a saved favorite.
    pub fn edit(fav: &Favorite) -> Form {
        let s = &fav.site;
        let mut f = Form::new(s.protocol);
        f.editing = Some(s.id.clone());
        f.name = TextEdit::new(s.name.clone());
        f.host = TextEdit::new(s.host.clone());
        f.port = TextEdit::new(s.port.map(|p| p.to_string()).unwrap_or_default());
        f.user = TextEdit::new(s.user.clone());
        f.path = TextEdit::new(s.initial_path.clone().unwrap_or_default());
        f.region = TextEdit::new(s.region.clone().unwrap_or_default());
        f.project = TextEdit::new(s.project.clone().unwrap_or_default());
        f.domain = TextEdit::new(s.domain.clone().unwrap_or_default());
        f.tls = s.tls;
        f.passive = s.passive;
        f.path_style = s.path_style;
        f.folder = fav.folder.clone();
        f.trusted = s.trusted_fingerprints.clone();
        match &s.auth {
            Auth::Password { .. } => f.has_saved_secret = true,
            Auth::KeyFile { path, passphrase } => {
                f.auth = AuthChoice::KeyFile;
                f.key_path = TextEdit::new(path.clone());
                f.has_saved_secret = passphrase.is_some();
            }
            Auth::Agent => f.auth = AuthChoice::Agent,
            Auth::OAuth { .. } => {
                f.has_saved_secret = true;
                f.oauth_account = Some(s.user.clone()).filter(|u| !u.is_empty());
            }
            Auth::None => {}
        }
        f
    }

    pub fn set_protocol(&mut self, p: Protocol) {
        self.protocol = p;
        self.errors.clear();
        if p == Protocol::Ftps && self.port.text.is_empty() && self.tls == TlsMode::Implicit {
            self.port = TextEdit::new("990");
        }
    }

    /// Fills the fields from a `~/.ssh/config` host.
    pub fn use_suggestion(&mut self, h: &SshHost) {
        self.set_protocol(Protocol::Sftp);
        self.name = TextEdit::new(h.alias.clone());
        self.host = TextEdit::new(h.host.clone());
        self.port = TextEdit::new(h.port.map(|p| p.to_string()).unwrap_or_default());
        if let Some(u) = &h.user {
            self.user = TextEdit::new(u.clone());
        }
        match &h.identity_file {
            Some(k) => {
                self.auth = AuthChoice::KeyFile;
                self.key_path = TextEdit::new(k.clone());
            }
            None => self.auth = AuthChoice::Agent,
        }
    }

    /// Which fields this protocol shows, in order.
    pub fn fields(&self) -> Vec<Field> {
        use Field::*;
        let mut v = vec![Name];
        match self.protocol {
            Protocol::Local => {}
            Protocol::Sftp => {
                v.extend([Host, Port, User]);
                match self.auth {
                    AuthChoice::Password => v.push(Password),
                    AuthChoice::KeyFile => v.extend([KeyPath, Password]),
                    AuthChoice::Agent => {}
                }
                v.push(Path);
            }
            Protocol::Ftp | Protocol::Ftps | Protocol::WebDav | Protocol::WebDavs => v.extend([Host, Port, User, Password, Path]),
            Protocol::S3 => v.extend([Host, Region, User, Password, Path]),
            Protocol::B2 => v.extend([User, Password, Path]),
            Protocol::AzureBlob => v.extend([User, Password, Path]),
            Protocol::Swift => v.extend([Host, User, Password, Project, Domain, Region, Path]),
            Protocol::GoogleDrive | Protocol::Dropbox | Protocol::OneDrive | Protocol::Box => v.extend([SignIn, Path]),
        }
        v
    }

    /// The label and placeholder for a field under this protocol.
    pub fn label(&self, f: Field) -> (&'static str, &'static str) {
        let p = self.protocol;
        match f {
            Field::Name => ("Name", "Shown in Favorites"),
            Field::Host => match p {
                Protocol::S3 => ("Endpoint", "Leave empty for Amazon S3, or https://minio.local:9000"),
                Protocol::Swift => ("Auth URL", "https://identity.example.com/v3"),
                Protocol::WebDav | Protocol::WebDavs => ("Server", "dav.example.com"),
                _ => ("Server", "example.com"),
            },
            Field::Port => ("Port", match p {
                Protocol::Sftp => "22",
                Protocol::Ftps if self.tls == TlsMode::Implicit => "990",
                Protocol::Ftp | Protocol::Ftps => "21",
                Protocol::WebDav => "80",
                _ => "443",
            }),
            Field::User => match p {
                Protocol::S3 => ("Access key ID", "AKIA…"),
                Protocol::B2 => ("Key ID", "0012ab…"),
                Protocol::AzureBlob => ("Storage account", "mystorageaccount"),
                Protocol::Ftp | Protocol::Ftps => ("User", "Leave empty to log in anonymously"),
                _ => ("User", "deploy"),
            },
            Field::Password => match p {
                Protocol::S3 => ("Secret access key", stored_hint(false)),
                Protocol::B2 => ("Application key", stored_hint(false)),
                Protocol::AzureBlob => ("Account key", stored_hint(false)),
                Protocol::Swift => ("API key or password", stored_hint(false)),
                Protocol::Sftp if self.auth == AuthChoice::KeyFile => ("Key passphrase", "Only if the key has one"),
                _ => ("Password", stored_hint(self.has_saved_secret)),
            },
            Field::KeyPath => ("Private key", if cfg!(windows) { "~\\.ssh\\id_ed25519" } else { "~/.ssh/id_ed25519" }),
            Field::Region => ("Region", if p == Protocol::S3 { "us-east-1" } else { "Optional" }),
            Field::Project => ("Project", "Needed for Keystone v3, e.g. demo"),
            Field::Domain => ("Domain", "Default"),
            Field::Path => match p {
                Protocol::S3 | Protocol::B2 => ("Bucket and folder", "Optional: my-bucket/site"),
                Protocol::AzureBlob | Protocol::Swift => ("Container and folder", "Optional: assets/2026"),
                _ => ("Initial folder", "Optional: /var/www"),
            },
            Field::SignIn => ("Account", ""),
        }
    }

    pub fn edit_mut(&mut self, f: Field) -> &mut TextEdit {
        match f {
            Field::Name => &mut self.name,
            Field::Host => &mut self.host,
            Field::Port => &mut self.port,
            Field::User => &mut self.user,
            Field::Password => &mut self.password,
            Field::KeyPath => &mut self.key_path,
            Field::Region => &mut self.region,
            Field::Project => &mut self.project,
            Field::Domain => &mut self.domain,
            Field::Path | Field::SignIn => &mut self.path,
        }
    }

    pub fn edit_ref(&self, f: Field) -> &TextEdit {
        match f {
            Field::Name => &self.name,
            Field::Host => &self.host,
            Field::Port => &self.port,
            Field::User => &self.user,
            Field::Password => &self.password,
            Field::KeyPath => &self.key_path,
            Field::Region => &self.region,
            Field::Project => &self.project,
            Field::Domain => &self.domain,
            Field::Path | Field::SignIn => &self.path,
        }
    }

    /// Checks every field; fills `errors` and returns whether the form can connect.
    pub fn validate(&mut self) -> bool {
        self.errors.clear();
        let p = self.protocol;
        let fields = self.fields();
        let has = |f: Field| fields.contains(&f);
        let host = self.host.text.trim();
        let needs_host = matches!(p, Protocol::Sftp | Protocol::Ftp | Protocol::Ftps | Protocol::WebDav | Protocol::WebDavs | Protocol::Swift);
        if has(Field::Host) {
            if host.is_empty() && needs_host {
                self.errors.insert(Field::Host, match p {
                    Protocol::Swift => "Enter the auth URL.".into(),
                    _ => "Enter the server's name or address.".into(),
                });
            } else if host.contains(char::is_whitespace) {
                self.errors.insert(Field::Host, "A server address can't contain spaces.".into());
            } else if p == Protocol::Swift && !host.contains("://") {
                self.errors.insert(Field::Host, "Use a full URL starting with https://.".into());
            }
        }
        if has(Field::Port) && !self.port.text.trim().is_empty() {
            match self.port.text.trim().parse::<u32>() {
                Ok(n) if (1..=65535).contains(&n) => {}
                _ => {
                    self.errors.insert(Field::Port, "A port is a number from 1 to 65535.".into());
                }
            }
        }
        let user_required = matches!(p, Protocol::Sftp | Protocol::S3 | Protocol::B2 | Protocol::AzureBlob | Protocol::Swift);
        if has(Field::User) && user_required && self.user.text.trim().is_empty() {
            let (label, _) = self.label(Field::User);
            self.errors.insert(Field::User, format!("Enter the {}.", label.to_lowercase()));
        }
        let secret_required = matches!(p, Protocol::S3 | Protocol::B2 | Protocol::AzureBlob | Protocol::Swift);
        if has(Field::Password) && secret_required && self.password.text.is_empty() && !self.has_saved_secret {
            let (label, _) = self.label(Field::Password);
            self.errors.insert(Field::Password, format!("Enter the {}.", label.to_lowercase()));
        }
        if has(Field::Project) && self.project.text.trim().contains(':') {
            self.errors.insert(Field::Project, "Enter just the project name; the user goes in User.".into());
        }
        if has(Field::User) && p == Protocol::Swift && self.user.text.contains(':') && self.project.text.trim().is_empty() {
            self.errors.insert(Field::User, "Put the project in Project and only the user name here.".into());
        }
        if has(Field::KeyPath) && self.key_path.text.trim().is_empty() {
            self.errors.insert(Field::KeyPath, "Choose the private key file.".into());
        }
        if has(Field::SignIn) && self.oauth_token.is_none() && !self.has_saved_secret {
            self.errors.insert(Field::SignIn, format!("Sign in with {} first.", p.label()));
        }
        self.errors.is_empty()
    }

    /// The site these fields describe, under `id`, and the secrets to put in the Keychain
    /// (account, value). An empty secret field on an edited favorite keeps the saved one.
    pub fn to_site(&self, id: Option<&str>) -> (Site, Vec<(String, String)>) {
        let id = id.map(str::to_owned).or_else(|| self.editing.clone()).unwrap_or_else(new_id);
        let p = self.protocol;
        let host = self.host.text.trim().to_owned();
        let user = self.user.text.trim().to_owned();
        let name = Some(self.name.text.trim().to_owned()).filter(|n| !n.is_empty()).unwrap_or_else(|| match p {
            _ if !host.is_empty() && matches!(p, Protocol::Sftp | Protocol::Ftp | Protocol::Ftps | Protocol::WebDav | Protocol::WebDavs) => {
                host.clone()
            }
            Protocol::AzureBlob if !user.is_empty() => user.clone(),
            _ => p.label().to_owned(),
        });
        let mut site = Site::new(id.clone(), name, p);
        site.host = if p == Protocol::AzureBlob { user.clone() } else { host };
        site.port = self.port.text.trim().parse().ok();
        site.user = if p.uses_oauth() { self.oauth_account.clone().unwrap_or_default() } else { user };
        site.initial_path = Some(self.path.text.trim().to_owned()).filter(|s| !s.is_empty());
        site.region = Some(self.region.text.trim().to_owned()).filter(|s| !s.is_empty());
        site.tls = self.tls;
        site.passive = self.passive;
        site.path_style = self.path_style;
        if p == Protocol::Swift {
            site.project = Some(self.project.text.trim().to_owned()).filter(|s| !s.is_empty());
            site.domain = Some(self.domain.text.trim().to_owned()).filter(|s| !s.is_empty());
        }
        site.trusted_fingerprints = self.trusted.clone();
        let mut secrets = Vec::new();
        let mut secret = |what: &str, value: &str, keep: bool| -> Option<SecretRef> {
            let account = secret_account(&id, what);
            if !value.is_empty() {
                secrets.push((account.clone(), value.to_owned()));
                Some(SecretRef(account))
            } else if keep {
                Some(SecretRef(account))
            } else {
                None
            }
        };
        site.auth = if p.uses_oauth() {
            match secret("oauth", self.oauth_token.as_deref().unwrap_or(""), self.has_saved_secret) {
                Some(r) => Auth::OAuth { refresh_token: r },
                None => Auth::None,
            }
        } else if p == Protocol::Sftp && self.auth == AuthChoice::Agent {
            Auth::Agent
        } else if p == Protocol::Sftp && self.auth == AuthChoice::KeyFile {
            let path = expand_home(self.key_path.text.trim());
            Auth::KeyFile { path, passphrase: secret("passphrase", &self.password.text, self.has_saved_secret) }
        } else {
            match secret("password", &self.password.text, self.has_saved_secret) {
                Some(r) => Auth::Password { secret: r },
                None => Auth::None,
            }
        };
        (site, secrets)
    }
}

/// `~/x` (or `~\\x` on Windows) to the home folder's `x`.
pub fn expand_home(p: &str) -> String {
    filewinch_os::dirs::expand_tilde(p).to_string_lossy().into_owned()
}

/// Where a typed secret goes, per OS: "Stored in your Keychain", "Stored in Windows
/// Credential Manager", "Stored in your keyring".
fn stored_hint(saved: bool) -> &'static str {
    use filewinch_os::Os;
    match (crate::theme::ui_os(), saved) {
        (Os::Mac, false) => "Stored in your Keychain",
        (Os::Mac, true) => "Saved in your Keychain",
        (Os::Windows, false) => "Stored in Windows Credential Manager",
        (Os::Windows, true) => "Saved in Windows Credential Manager",
        (Os::Linux, false) => "Stored in your keyring",
        (Os::Linux, true) => "Saved in your keyring",
    }
}
