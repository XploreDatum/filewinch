//! Favorites: saved sites, optionally inside one level of folders.

use filewinch_core::{Auth, SecretRef, Site};
use serde::{Deserialize, Serialize};

use crate::secrets::secret_account;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub collapsed: bool,
}

/// A saved site. "Always trust" fingerprints live on the site itself
/// (`Site::trusted_fingerprints`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "FavoriteRecord")]
pub struct Favorite {
    #[serde(with = "crate::site_serde")]
    pub site: Site,
    /// The folder it sits in; `None` is the top level.
    #[serde(default)]
    pub folder: Option<String>,
}

/// What `sites.json` may hold: earlier builds kept trusted fingerprints beside the site as
/// `trusted`; they move onto the site on read.
#[derive(Deserialize)]
struct FavoriteRecord {
    #[serde(with = "crate::site_serde")]
    site: Site,
    #[serde(default)]
    folder: Option<String>,
    #[serde(default)]
    trusted: Vec<String>,
}

impl From<FavoriteRecord> for Favorite {
    fn from(r: FavoriteRecord) -> Favorite {
        let mut site = r.site;
        if !r.trusted.is_empty() {
            let all = std::mem::take(&mut site.trusted_fingerprints).into_iter().chain(r.trusted);
            site.trusted_fingerprints = crate::site_serde::normalized_fingerprints(all);
        }
        Favorite { site, folder: r.folder }
    }
}

impl Favorite {
    pub fn new(site: Site, folder: Option<String>) -> Favorite {
        Favorite { site, folder }
    }
}

/// Everything in `sites.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SitesFile {
    #[serde(default)]
    pub folders: Vec<Folder>,
    #[serde(default)]
    pub favorites: Vec<Favorite>,
}

/// A new unique id: time and a counter, in hex.
pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_micros());
    format!("{t:x}{:04x}", N.fetch_add(1, Ordering::Relaxed) & 0xffff)
}

impl SitesFile {
    pub fn get(&self, id: &str) -> Option<&Favorite> {
        self.favorites.iter().find(|f| f.site.id == id)
    }

    /// Adds or replaces (by site id).
    pub fn upsert(&mut self, fav: Favorite) {
        match self.favorites.iter_mut().find(|f| f.site.id == fav.site.id) {
            Some(f) => *f = fav,
            None => self.favorites.push(fav),
        }
    }

    /// Removes a favorite and returns it, so the caller can delete its Keychain secrets.
    pub fn remove(&mut self, id: &str) -> Option<Favorite> {
        let i = self.favorites.iter().position(|f| f.site.id == id)?;
        Some(self.favorites.remove(i))
    }

    /// A copy of `id` named "<name> copy" right after it, with fresh secret names. Returns the
    /// copy and `(from, to)` Keychain accounts the caller copies.
    pub fn duplicate(&mut self, id: &str) -> Option<(Favorite, Vec<(SecretRef, SecretRef)>)> {
        let i = self.favorites.iter().position(|f| f.site.id == id)?;
        let mut copy = self.favorites[i].clone();
        let new = new_id();
        copy.site.id = new.clone();
        copy.site.name = self.unique_name(&format!("{} copy", self.favorites[i].site.name));
        let mut moves = Vec::new();
        let mut rename = |r: &mut SecretRef, what: &str| {
            let to = SecretRef(secret_account(&new, what));
            moves.push((r.clone(), to.clone()));
            *r = to;
        };
        match &mut copy.site.auth {
            Auth::Password { secret } => rename(secret, "password"),
            Auth::KeyFile { passphrase: Some(p), .. } => rename(p, "passphrase"),
            Auth::OAuth { refresh_token } => rename(refresh_token, "oauth"),
            _ => {}
        }
        self.favorites.insert(i + 1, copy.clone());
        Some((copy, moves))
    }

    /// `name`, or `name 2`, `name 3`… so no two favorites share a name.
    pub fn unique_name(&self, name: &str) -> String {
        let taken = |n: &str| self.favorites.iter().any(|f| f.site.name == n);
        if !taken(name) {
            return name.to_owned();
        }
        (2..).map(|i| format!("{name} {i}")).find(|n| !taken(n)).unwrap_or_else(|| name.to_owned())
    }

    pub fn add_folder(&mut self, name: &str) -> String {
        let id = new_id();
        self.folders.push(Folder { id: id.clone(), name: name.to_owned(), collapsed: false });
        id
    }

    pub fn rename_folder(&mut self, id: &str, name: &str) -> bool {
        self.folders.iter_mut().find(|f| f.id == id).map(|f| f.name = name.to_owned()).is_some()
    }

    /// Removes a folder; its favorites move to the top level.
    pub fn remove_folder(&mut self, id: &str) {
        self.folders.retain(|f| f.id != id);
        for f in &mut self.favorites {
            if f.folder.as_deref() == Some(id) {
                f.folder = None;
            }
        }
    }

    /// Moves a favorite into `folder` (`None`: top level). Unknown folders mean top level.
    pub fn move_to(&mut self, id: &str, folder: Option<&str>) {
        let folder = folder.filter(|f| self.folders.iter().any(|x| x.id == *f)).map(str::to_owned);
        if let Some(f) = self.favorites.iter_mut().find(|f| f.site.id == id) {
            f.folder = folder;
        }
    }

    /// Favorites whose name, host or user contains `query` (any case). Empty matches all.
    pub fn search<'a>(&'a self, query: &str) -> impl Iterator<Item = &'a Favorite> + 'a {
        let q = query.trim().to_lowercase();
        self.favorites.iter().filter(move |f| {
            q.is_empty()
                || [&f.site.name, &f.site.host, &f.site.user].iter().any(|s| s.to_lowercase().contains(&q))
                || f.site.protocol.label().to_lowercase().contains(&q)
        })
    }
}
