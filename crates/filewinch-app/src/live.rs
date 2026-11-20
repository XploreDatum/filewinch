//! The `remote-live` headless scene: the right pane connected to a real server, for
//! screenshots of a real listing (`scripts/snap-remote-live.sh` starts the server).
//!
//! The server comes from environment variables, so nothing about it lives in the repository:
//! `FILEWINCH_LIVE_PROTOCOL` (`sftp`, `ftp`, `webdav`; default `sftp`), `FILEWINCH_LIVE_HOST`
//! (default `127.0.0.1`), `FILEWINCH_LIVE_PORT`, `FILEWINCH_LIVE_USER`,
//! `FILEWINCH_LIVE_PASSWORD`, `FILEWINCH_LIVE_FINGERPRINT` (the host key to trust, SFTP) and
//! `FILEWINCH_LIVE_PATH` (folder to open). The password is held in memory only; headless runs
//! never touch the Keychain. A failed connection shows the app's own error sheet.

use std::sync::Arc;

use filewinch_core::{Auth, Error, ErrorKind, MemorySecrets, Protocol, RPath, Remote, SecretRef, Site};

use crate::connect::flow::{Conn, Target};
use crate::pane::Source;
use crate::registry;
use crate::ui::{Session, Ui, next_session_id};

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn site() -> Site {
    let protocol = match var("FILEWINCH_LIVE_PROTOCOL").as_deref().unwrap_or("sftp") {
        "ftp" => Protocol::Ftp,
        "webdav" => Protocol::WebDav,
        _ => Protocol::Sftp,
    };
    let host = var("FILEWINCH_LIVE_HOST").unwrap_or_else(|| "127.0.0.1".into());
    let mut site = Site::new("live", format!("{} on {host}", protocol.label()), protocol);
    site.host = host;
    site.port = var("FILEWINCH_LIVE_PORT").and_then(|p| p.parse().ok());
    site.user = var("FILEWINCH_LIVE_USER").unwrap_or_default();
    site.auth = Auth::Password { secret: SecretRef("live/password".into()) };
    site.initial_path = var("FILEWINCH_LIVE_PATH");
    site.trusted_fingerprints = var("FILEWINCH_LIVE_FINGERPRINT").into_iter().collect();
    site
}

fn open(site: &Site) -> Result<(Arc<dyn Remote>, RPath), Error> {
    let connector = registry::connector(site.protocol)
        .ok_or_else(|| Error::new(ErrorKind::Unsupported, format!("This build can't connect with {}.", site.protocol.label())))?;
    let secrets = MemorySecrets::new().with("live/password", &var("FILEWINCH_LIVE_PASSWORD").unwrap_or_default());
    let remote: Arc<dyn Remote> = Arc::from(connector.connect(site, Arc::new(secrets))?);
    let start = match &site.initial_path {
        Some(p) => RPath::new(p),
        None => remote.home()?,
    };
    Ok((remote, start))
}

/// Connects pane `p` to the server the environment describes.
pub fn connect_pane(s: &mut Ui, p: usize) {
    let site = site();
    match open(&site) {
        Ok((remote, start)) => {
            let id = next_session_id();
            s.sessions.push(Session { id, site: site.clone(), remote: remote.clone() });
            let hidden = s.prefs.show_hidden;
            s.panes[p].switch_to(Source { session: id, label: site.name.clone(), protocol: site.protocol, remote }, start, hidden);
            s.active = p;
        }
        Err(error) => {
            eprintln!("remote-live: couldn't connect: {}", error.message);
            let target = Target { site, pane: p, secrets: Vec::new(), save: false, folder: None, trusted_once: Vec::new() };
            s.conn = Conn::Failed { target, error, details: true };
        }
    }
}
