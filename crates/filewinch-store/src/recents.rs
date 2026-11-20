//! Recent connections, newest first.

use filewinch_core::Site;
use serde::{Deserialize, Serialize};

pub const MAX_RECENTS: usize = 12;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recent {
    #[serde(with = "crate::site_serde")]
    pub site: Site,
    /// Seconds since the Unix epoch.
    pub at: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recents {
    #[serde(default)]
    pub items: Vec<Recent>,
}

impl Recents {
    /// Puts `site` at the top, replacing an earlier entry for the same site (by id, or by
    /// protocol, host, port and user for unsaved connections), and keeps the newest twelve.
    pub fn touch(&mut self, site: &Site, at: u64) {
        let same = |a: &Site| {
            a.id == site.id
                || (a.protocol == site.protocol && a.host == site.host && a.port == site.port && a.user == site.user)
        };
        self.items.retain(|r| !same(&r.site));
        self.items.insert(0, Recent { site: site.clone(), at });
        self.items.truncate(MAX_RECENTS);
    }
}
