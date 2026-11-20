//! Connecting: resolve the site against `~/.ssh/config`, open SSH, verify the host key,
//! authenticate, start the SFTP subsystem.

use crate::{config, errors, hostkey};
use filewinch_core::{Auth, Error, ErrorKind, Result, SecretRef, Site};
use russh::client::{self, AuthResult, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate, load_secret_key};
use russh_sftp::client::SftpSession;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// How to sign in, with secrets already fetched from the Keychain.
#[derive(Clone)]
pub(crate) enum Login {
    Password(String),
    Key { path: PathBuf, passphrase: Option<String> },
    Agent,
    /// Agent, then these identity files (unencrypted ones only).
    Default(Vec<PathBuf>),
}

#[derive(Clone)]
pub(crate) struct Params {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub login: Login,
    /// The site's "always trust" host-key fingerprints.
    pub trusted: Vec<String>,
}

impl Params {
    pub fn who(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }

    pub fn resolve(site: &Site, secret: &dyn Fn(&SecretRef) -> Result<Option<String>>) -> Result<Params> {
        let alias = site.host.trim();
        if alias.is_empty() {
            return Err(Error::new(ErrorKind::Other, "Enter a server address."));
        }
        let cfg = config::resolve(alias);
        let host = cfg.hostname.clone().unwrap_or_else(|| alias.to_string());
        let port = site.port.or(cfg.port).unwrap_or(22);
        let user = Some(site.user.trim().to_string())
            .filter(|u| !u.is_empty())
            .or(cfg.user.clone())
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_default();
        let who = format!("{user}@{host}");
        let login = match &site.auth {
            Auth::Password { secret: s } => match secret(s)? {
                Some(p) => Login::Password(p),
                None => return Err(Error::new(ErrorKind::Auth, format!("There's no saved password for {who}."))),
            },
            Auth::KeyFile { path, passphrase } => Login::Key {
                path: expand_home(path),
                passphrase: match passphrase {
                    Some(s) => secret(s)?,
                    None => None,
                },
            },
            Auth::Agent => Login::Agent,
            Auth::None | Auth::OAuth { .. } => {
                let mut ids = cfg.identity_files.clone();
                if (!cfg.identities_only || ids.is_empty())
                    && let Some(dirs) = filewinch_os::dirs::Dirs::current() {
                        for n in ["id_ed25519", "id_ecdsa", "id_rsa"] {
                            ids.push(dirs.ssh().join(n));
                        }
                    }
                Login::Default(ids)
            }
        };
        Ok(Params { host, port, user, login, trusted: site.trusted_fingerprints.clone() })
    }
}

fn expand_home(p: &str) -> PathBuf {
    filewinch_os::dirs::expand_tilde(p)
}

/// russh callbacks: only host key checking matters.
pub(crate) struct Client {
    host: String,
    port: u16,
    trusted: Vec<String>,
    verdict: Arc<Mutex<Option<Error>>>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> std::result::Result<bool, Self::Error> {
        let result = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => hostkey::verify(&self.host, self.port, key, &self.trusted),
            PublicKeyOrCertificate::Certificate(cert) => {
                // No CA support: the certificate's own key can still be trusted by fingerprint.
                let fp = cert.public_key().fingerprint(russh::keys::HashAlg::Sha256).to_string();
                if filewinch_core::is_trusted(&self.trusted, &fp) {
                    Ok(())
                } else {
                    Err(Error::host_trust(
                        format!("{} presented a host certificate; Filewinch doesn't check certificate authorities. Trust its key only if the fingerprint matches what the server's administrator gave you.", self.host),
                        &fp,
                        "host certificate",
                    ))
                }
            }
        };
        match result {
            Ok(()) => Ok(true),
            Err(e) => {
                *self.verdict.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                Ok(false)
            }
        }
    }
}

pub(crate) struct Conn {
    pub handle: Handle<Client>,
    pub sftp: SftpSession,
}

/// Opens one authenticated SSH connection with an SFTP channel.
pub(crate) async fn open(p: &Params) -> Result<Conn> {
    let who = if p.port == 22 { p.host.clone() } else { format!("{}:{}", p.host, p.port) };
    let cfg = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(15)),
        keepalive_max: 4,
        inactivity_timeout: None,
        nodelay: true,
        ..Default::default()
    });
    let verdict = Arc::new(Mutex::new(None));
    let handler = Client { host: p.host.clone(), port: p.port, trusted: p.trusted.clone(), verdict: verdict.clone() };
    let connected = tokio::time::timeout(CONNECT_TIMEOUT, client::connect(cfg, (p.host.as_str(), p.port), handler)).await;
    let mut handle = match connected {
        Err(_) => return Err(Error::new(ErrorKind::Network, format!("{who} didn't answer in time."))),
        Ok(Err(e)) => {
            if let Some(v) = verdict.lock().unwrap_or_else(|x| x.into_inner()).take() {
                return Err(v);
            }
            return Err(errors::ssh(e, &who));
        }
        Ok(Ok(h)) => h,
    };
    authenticate(&mut handle, p).await?;
    let channel = handle.channel_open_session().await.map_err(|e| errors::ssh(e, &who))?;
    channel.request_subsystem(true, "sftp").await.map_err(|e| errors::ssh(e, &who))?;
    let sftp_cfg = russh_sftp::client::Config { request_timeout_secs: 30, ..Default::default() };
    let sftp = SftpSession::new_with_config(channel.into_stream(), sftp_cfg)
        .await
        .map_err(|e| errors::sftp(e, "start SFTP"))?;
    Ok(Conn { handle, sftp })
}

async fn authenticate(h: &mut Handle<Client>, p: &Params) -> Result<()> {
    let who = p.who();
    let net = |e: russh::Error| errors::ssh(e, &p.host);
    match &p.login {
        Login::Password(pw) => {
            if h.authenticate_password(&p.user, pw).await.map_err(net)?.success() {
                return Ok(());
            }
            if keyboard_interactive(h, &p.user, pw).await.map_err(net)? {
                return Ok(());
            }
            Err(Error::new(ErrorKind::Auth, format!("The server refused the password for {who}.")))
        }
        Login::Key { path, passphrase } => {
            let key = load_key(path, passphrase.as_deref())?;
            if publickey(h, &p.user, key).await.map_err(net)? {
                return Ok(());
            }
            Err(Error::new(ErrorKind::Auth, format!("The server didn't accept the key {} for {who}.", path.display())))
        }
        Login::Agent => {
            let tried = agent(h, &p.user).await?;
            if tried.0 {
                return Ok(());
            }
            let msg = if tried.1 == 0 {
                "The SSH agent has no keys. Add one with ssh-add.".to_string()
            } else {
                format!("The server didn't accept any of the {} keys in the SSH agent for {who}.", tried.1)
            };
            Err(Error::new(ErrorKind::Auth, msg))
        }
        Login::Default(ids) => {
            if agent_configured() && agent(h, &p.user).await.is_ok_and(|t| t.0) {
                return Ok(());
            }
            for path in ids.iter().filter(|p| p.exists()) {
                let Ok(key) = load_key(path, None) else { continue };
                if publickey(h, &p.user, key).await.map_err(net)? {
                    return Ok(());
                }
            }
            if h.authenticate_none(&p.user).await.map_err(net)?.success() {
                return Ok(());
            }
            Err(Error::new(ErrorKind::Auth, format!("The server needs a password or key for {who}.")))
        }
    }
}

fn load_key(path: &PathBuf, passphrase: Option<&str>) -> Result<russh::keys::PrivateKey> {
    let shown = path.display();
    if !path.exists() {
        return Err(Error::new(ErrorKind::Auth, format!("The key file {shown} doesn't exist.")));
    }
    load_secret_key(path, passphrase).map_err(|e| {
        let msg = match (&e, passphrase) {
            (russh::keys::Error::KeyIsEncrypted, _) => format!("The key {shown} is protected by a passphrase. Enter it to continue."),
            (_, Some(_)) => format!("Couldn't unlock the key {shown}. Check the passphrase."),
            _ => format!("Couldn't read the key {shown}."),
        };
        Error::new(ErrorKind::Auth, msg).with_detail(e)
    })
}

async fn publickey(h: &mut Handle<Client>, user: &str, key: russh::keys::PrivateKey) -> std::result::Result<bool, russh::Error> {
    let hash = if key.algorithm().is_rsa() { h.best_supported_rsa_hash().await?.flatten() } else { None };
    let r = h.authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await?;
    Ok(r.success())
}

/// Whether an SSH agent may be running: `SSH_AUTH_SOCK` on macOS and Linux; on Windows the
/// OpenSSH agent service or Pageant may run without any variable set, so always try.
fn agent_configured() -> bool {
    cfg!(windows) || std::env::var_os("SSH_AUTH_SOCK").is_some()
}

type Agent = AgentClient<Box<dyn AgentStream + Send + Unpin + 'static>>;

/// The agent at `SSH_AUTH_SOCK` (a Unix socket).
#[cfg(unix)]
async fn connect_agent() -> Result<Agent> {
    let agent = AgentClient::connect_env().await.map_err(|e| {
        Error::new(ErrorKind::Auth, "Couldn't reach the SSH agent. Is SSH_AUTH_SOCK set?").with_detail(e)
    })?;
    Ok(agent.dynamic())
}

/// The OpenSSH for Windows agent (`\\.\pipe\openssh-ssh-agent`, or the pipe `SSH_AUTH_SOCK`
/// names), then Pageant.
#[cfg(windows)]
async fn connect_agent() -> Result<Agent> {
    let pipe = std::env::var_os("SSH_AUTH_SOCK")
        .filter(|p| p.to_string_lossy().starts_with(r"\\.\pipe\"))
        .unwrap_or_else(|| r"\\.\pipe\openssh-ssh-agent".into());
    let pipe_err = match AgentClient::connect_named_pipe(&pipe).await {
        Ok(agent) => return Ok(agent.dynamic()),
        Err(e) => e,
    };
    match AgentClient::connect_pageant().await {
        Ok(agent) => Ok(agent.dynamic()),
        Err(pageant_err) => Err(Error::new(
            ErrorKind::Auth,
            "Couldn't reach an SSH agent. Start the “OpenSSH Authentication Agent” service (Services app) or Pageant, then add your key.",
        )
        .with_detail(format!("{}: {pipe_err}; Pageant: {pageant_err}", pipe.to_string_lossy()))),
    }
}

/// Tries every agent key. Returns (signed in, keys tried).
async fn agent(h: &mut Handle<Client>, user: &str) -> Result<(bool, usize)> {
    let mut agent = connect_agent().await?;
    let ids = agent
        .request_identities()
        .await
        .map_err(|e| Error::new(ErrorKind::Auth, "The SSH agent didn't list its keys.").with_detail(e))?;
    let mut tried = 0;
    for id in ids {
        let AgentIdentity::PublicKey { key, .. } = id else { continue };
        tried += 1;
        let hash = if key.algorithm().is_rsa() {
            h.best_supported_rsa_hash().await.map_err(|e| errors::ssh(e, user))?.flatten()
        } else {
            None
        };
        match h.authenticate_publickey_with(user, key, hash, &mut agent).await {
            Ok(AuthResult::Success) => return Ok((true, tried)),
            Ok(_) => {}
            Err(_) => {} // the agent couldn't sign with this key; try the next
        }
    }
    Ok((false, tried))
}

/// Answers keyboard-interactive prompts with the password (servers that disable plain
/// password auth but ask "Password:" this way).
async fn keyboard_interactive(h: &mut Handle<Client>, user: &str, pw: &str) -> std::result::Result<bool, russh::Error> {
    let mut r = h.authenticate_keyboard_interactive_start(user, None).await?;
    for _ in 0..5 {
        match r {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(false),
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                let answers = prompts.iter().map(|q| if q.echo { String::new() } else { pw.to_string() }).collect();
                r = h.authenticate_keyboard_interactive_respond(answers).await?;
            }
        }
    }
    Ok(false)
}
