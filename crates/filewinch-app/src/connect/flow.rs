//! Connecting: one state machine for the attempt, its cancel, a password re-prompt after an
//! auth failure, and the trust question for an unknown host key or certificate.

use std::sync::Arc;

use filewinch_core::{Error, ErrorKind, RPath, Remote, Result, SecretRef, Site};
use filewinch_store::{Favorite, SecretStore, secret_account};
use rune::TextEdit;
use rune::machine::Attempt;

use crate::hub::hub;
use crate::pane::Source;
use crate::registry;
use crate::ui::{Session, Sheet, Tone, Ui, next_session_id};

/// The secrets an attempt resolves first (typed into the sheet), before the Keychain.
pub type Secrets = Vec<(String, String)>;

/// Where a connection attempt goes when it succeeds.
#[derive(Clone)]
pub struct Target {
    pub site: Site,
    pub pane: usize,
    pub secrets: Secrets,
    /// Save as a favorite (and its secrets in the Keychain) once connected.
    pub save: bool,
    pub folder: Option<String>,
    /// Fingerprints trusted for this attempt only ("Trust Once"); "Always Trust" ones live in
    /// `site.trusted_fingerprints` and are saved with the favorite.
    pub trusted_once: Vec<String>,
}

#[allow(clippy::large_enum_variant)] // one value, held in the window's state
pub enum Conn {
    Idle,
    Connecting { attempt: Attempt, target: Target },
    Failed { target: Target, error: Error, details: bool },
    Password { target: Target, edit: TextEdit, message: String },
    Trust { target: Target, fingerprint: String, reason: String },
}

impl Conn {
    pub fn is_idle(&self) -> bool {
        matches!(self, Conn::Idle)
    }
}

/// The canonical fingerprint and the reason from a HostTrust error (its `detail` is
/// `"SHA256:…; reason"`, see `filewinch_core::Error::host_trust`).
pub fn parse_trust(e: &Error) -> (String, String) {
    let detail = e.detail.clone().unwrap_or_default();
    match e.fingerprint() {
        Some(fp) => (fp, detail.split_once(';').map(|(_, why)| why.trim().to_owned()).unwrap_or_default()),
        None => (String::new(), detail),
    }
}

/// What a session reads its secrets from: the values typed into the sheet first, then the
/// Keychain. A rotated OAuth token is written back to the Keychain when the account is saved
/// there (or the site is being saved); otherwise it stays in memory for this session.
pub struct AttemptSecrets {
    typed: std::sync::Mutex<Secrets>,
    store: Arc<dyn SecretStore>,
    save: bool,
}

impl AttemptSecrets {
    pub fn new(typed: Secrets, store: Arc<dyn SecretStore>, save: bool) -> Self {
        AttemptSecrets { typed: std::sync::Mutex::new(typed), store, save }
    }

    fn typed(&self) -> std::sync::MutexGuard<'_, Secrets> {
        self.typed.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl filewinch_core::Secrets for AttemptSecrets {
    fn get(&self, r: &SecretRef) -> Result<Option<String>> {
        if let Some((_, v)) = self.typed().iter().find(|(a, _)| *a == r.0) {
            return Ok(Some(v.clone()));
        }
        self.store.get(&r.0)
    }

    fn set(&self, r: &SecretRef, value: &str) -> Result<()> {
        {
            let mut typed = self.typed();
            typed.retain(|(a, _)| *a != r.0);
            typed.push((r.0.clone(), value.to_owned()));
        }
        if self.save || self.store.get(&r.0)?.is_some() {
            self.store.set(&r.0, value)?;
        }
        Ok(())
    }
}

fn open(site: &Site, secrets: &Secrets, trusted_once: &[String], save: bool) -> Result<(Arc<dyn Remote>, RPath)> {
    let connector = registry::connector(site.protocol).ok_or_else(|| {
        Error::new(ErrorKind::Unsupported, format!("This build of Filewinch can't connect with {} yet.", site.protocol.label()))
    })?;
    let mut site = site.clone();
    site.trusted_fingerprints.extend(trusted_once.iter().cloned());
    let site = &site;
    let secrets = Arc::new(AttemptSecrets::new(secrets.clone(), hub().secrets.clone(), save));
    let remote: Arc<dyn Remote> = Arc::from(connector.connect(site, secrets)?);
    let home = remote.home()?;
    let start = match &site.initial_path {
        Some(p) if !p.trim().is_empty() => {
            let p = if p.starts_with('/') { RPath::new(p) } else { home.join(p) };
            // A missing initial folder isn't fatal: open home and say so in the pane.
            match remote.stat(&p) {
                Ok(e) if e.is_dir() => p,
                _ => home,
            }
        }
        _ => home,
    };
    Ok((remote, start))
}

impl Ui {
    /// Starts connecting `target`; the result lands in [`Ui::connected`].
    pub fn connect(&mut self, target: Target) {
        let attempt = self.seq.next();
        let (site, secrets, once, save) = (target.site.clone(), target.secrets.clone(), target.trusted_once.clone(), target.save);
        self.conn = Conn::Connecting { attempt, target };
        self.run(move || open(&site, &secrets, &once, save), move |ui, res| ui.connected(attempt, res));
    }

    pub fn connect_favorite(&mut self, id: &str, pane: usize) {
        let Some(fav) = self.sites.get(id).cloned() else { return };
        self.sidebar_selected = Some(id.to_owned());
        self.connect(Target { site: fav.site, pane, secrets: Vec::new(), save: false, folder: fav.folder, trusted_once: Vec::new() });
    }

    /// Stops waiting for the attempt; a late answer is closed and dropped.
    pub fn cancel_connect(&mut self) {
        self.conn = Conn::Idle;
    }

    pub fn retry_connect(&mut self) {
        let target = match std::mem::replace(&mut self.conn, Conn::Idle) {
            Conn::Failed { target, .. } | Conn::Trust { target, .. } | Conn::Password { target, .. } => target,
            other => {
                self.conn = other;
                return;
            }
        };
        self.connect(target);
    }

    pub fn submit_password(&mut self) {
        let Conn::Password { target, edit, .. } = std::mem::replace(&mut self.conn, Conn::Idle) else { return };
        let mut target = target;
        let account = match &target.site.auth {
            filewinch_core::Auth::Password { secret } => secret.0.clone(),
            filewinch_core::Auth::KeyFile { passphrase: Some(p), .. } => p.0.clone(),
            filewinch_core::Auth::KeyFile { passphrase: None, path } => {
                let account = secret_account(&target.site.id, "passphrase");
                target.site.auth = filewinch_core::Auth::KeyFile { path: path.clone(), passphrase: Some(filewinch_core::SecretRef(account.clone())) };
                account
            }
            _ => {
                let account = secret_account(&target.site.id, "password");
                target.site.auth = filewinch_core::Auth::Password { secret: filewinch_core::SecretRef(account.clone()) };
                account
            }
        };
        target.secrets.retain(|(a, _)| *a != account);
        target.secrets.push((account, edit.text.clone()));
        // A favorite's new password replaces the saved one once it works.
        if self.sites.get(&target.site.id).is_some() {
            target.save = true;
        }
        self.connect(target);
    }

    pub fn trust(&mut self, always: bool) {
        let Conn::Trust { mut target, fingerprint, .. } = std::mem::replace(&mut self.conn, Conn::Idle) else { return };
        if !fingerprint.is_empty() {
            if !always {
                target.trusted_once.push(fingerprint);
            } else if !filewinch_core::is_trusted(&target.site.trusted_fingerprints, &fingerprint) {
                // Saved into the favorite (on the site) once the connection works.
                target.site.trusted_fingerprints.push(fingerprint);
                if self.sites.get(&target.site.id).is_some() {
                    target.save = true;
                }
            }
        }
        self.connect(target);
    }

    fn connected(&mut self, attempt: Attempt, res: Result<(Arc<dyn Remote>, RPath)>) {
        let target = match std::mem::replace(&mut self.conn, Conn::Idle) {
            Conn::Connecting { attempt: a, target } if a == attempt => target,
            other => {
                // Cancelled or superseded: close what the late answer opened.
                self.conn = other;
                if let Ok((remote, _)) = res {
                    remote.close();
                }
                return;
            }
        };
        match res {
            Ok((remote, start)) => self.open_session(target, remote, start),
            Err(error) if error.kind == ErrorKind::Auth => {
                let message = error.message.clone();
                self.conn = Conn::Password { target, edit: TextEdit::new(""), message };
            }
            Err(error) if error.kind == ErrorKind::HostTrust => {
                let (fingerprint, reason) = parse_trust(&error);
                let reason = if reason.is_empty() { error.message.clone() } else { reason };
                self.conn = Conn::Trust { target, fingerprint, reason };
            }
            Err(error) => self.conn = Conn::Failed { target, error, details: false },
        }
    }

    fn open_session(&mut self, target: Target, remote: Arc<dyn Remote>, start: RPath) {
        let Target { site, pane, secrets, save, folder, .. } = target;
        if save {
            let mut ok = true;
            for (account, value) in &secrets {
                if let Err(e) = hub().secrets.set(account, value) {
                    self.notify(Tone::Warning, format!("Connected, but the password couldn't be saved in {}.", crate::theme::ui_os().secret_store()), e.detail.clone().or(Some(e.message.clone())));
                    ok = false;
                }
            }
            let _ = ok;
            self.sites.upsert(Favorite { site: site.clone(), folder });
            self.save_sites();
            self.sidebar_selected = Some(site.id.clone());
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        self.recents.touch(&site, now);
        self.save_recents();
        let id = next_session_id();
        let label = site.name.clone();
        self.sessions.push(Session { id, site: site.clone(), remote: remote.clone() });
        let src = Source { session: id, label, protocol: site.protocol, remote };
        let hidden = self.prefs.show_hidden;
        self.panes[pane].switch_to(src, start, hidden);
        self.active = pane;
        if matches!(self.sheet, Some(Sheet::Connect(_))) {
            self.sheet = None;
        }
        // The site's matching local folder opens in the other pane, if it has one.
        if let Some(local) = site.local_path.as_deref().filter(|l| !l.is_empty()) {
            let other = self.other(pane);
            if self.panes[other].is_local() {
                self.panes[other].go(filewinch_local::rpath(&filewinch_os::dirs::expand_tilde(local)), hidden);
            }
        }
    }

    /// Empties pane `p` again (Connect… or This Mac); closes the session when no pane shows it.
    pub fn disconnect(&mut self, p: usize) {
        let Some(session) = self.panes[p].session().filter(|&s| s != 0) else { return };
        let hidden = self.prefs.show_hidden;
        self.panes[p].clear(hidden);
        self.close_if_unused(session);
    }

    /// Shows This Mac's home folder in pane `p`, closing the session it showed if no other pane
    /// shows it.
    pub fn show_local(&mut self, p: usize) {
        let old = self.panes[p].session();
        let hidden = self.prefs.show_hidden;
        let home = Source::local().remote.home().unwrap_or_else(|_| RPath::root());
        self.panes[p].switch_to(Source::local(), home, hidden);
        self.active = p;
        if let Some(session) = old.filter(|&s| s != 0) {
            self.close_if_unused(session);
        }
    }

    pub(crate) fn close_if_unused(&mut self, session: u64) {
        if self.panes.iter().all(|x| x.session() != Some(session)) {
            if let Some(i) = self.sessions.iter().position(|s| s.id == session) {
                let s = self.sessions.remove(i);
                let remote = s.remote.clone();
                self.run(move || remote.close(), |_, ()| {});
            }
            self.edits.list.retain(|e| e.session != session);
        }
    }
}
