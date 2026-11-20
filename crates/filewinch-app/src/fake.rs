//! An in-memory site for headless screenshots and tests: a web server's tree that lists,
//! reads, writes, renames and deletes like a real backend, without a network.

use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use filewinch_core::{Caps, Entry, EntryKind, Error, ErrorKind, RPath, Remote, Result, Shared, WriteSink};

#[derive(Clone)]
struct Node {
    dir: bool,
    data: Vec<u8>,
    mode: u32,
    modified: SystemTime,
}

#[derive(Default)]
pub struct FakeRemote {
    nodes: Arc<Mutex<BTreeMap<RPath, Node>>>,
    /// Where `home` points; the web root when unset.
    home: Option<RPath>,
    /// Items shared with others, as a cloud drive would mark them.
    shared: BTreeMap<RPath, Shared>,
}

/// A fixed moment in November 2026, India Standard Time (UTC+05:30), so scene data and the
/// "Today" it's shown against are the same every run. `day` may be any day of the year's
/// November or, below 1, earlier (`ist(0, 9, 40)` is 31 October).
pub fn ist(day: i64, hour: u64, min: u64) -> SystemTime {
    // 1 November 2026 00:00 UTC.
    const NOV_1: u64 = 1_793_491_200;
    let secs = NOV_1 as i64 + (day - 1) * 86_400 + (hour * 3600 + min * 60) as i64 - (5 * 3600 + 30 * 60);
    std::time::UNIX_EPOCH + Duration::from_secs(secs as u64)
}

/// "Now" for scenes: Friday 20 November 2026, 10:30 IST.
pub fn scene_now() -> SystemTime {
    ist(20, 10, 30)
}

impl FakeRemote {
    /// A tree of `files` (path, size, modified); their folders are made as needed, each dated
    /// by `dirs` when listed there, else by its newest file.
    pub fn from_files(files: &[(&str, usize, SystemTime)], dirs: &[(&str, SystemTime)]) -> FakeRemote {
        let f = FakeRemote::default();
        let Ok(mut n) = f.nodes.lock() else { return FakeRemote::default() };
        let epoch = std::time::UNIX_EPOCH;
        n.insert(RPath::root(), Node { dir: true, data: Vec::new(), mode: 0o755, modified: epoch });
        for (p, size, at) in files {
            let path = RPath::new(p);
            let mut parent = path.parent();
            while let Some(d) = parent {
                let node = n.entry(d.clone()).or_insert(Node { dir: true, data: Vec::new(), mode: 0o755, modified: epoch });
                node.modified = node.modified.max(*at);
                parent = d.parent();
            }
            n.insert(path, Node { dir: false, data: vec![b'x'; *size], mode: 0o644, modified: *at });
        }
        for (p, at) in dirs {
            n.entry(RPath::new(p)).or_insert(Node { dir: true, data: Vec::new(), mode: 0o755, modified: *at }).modified = *at;
        }
        drop(n);
        f
    }

    /// A small web root, `/var/www/site` with pages, styles and images, and `/var/www/logs`.
    /// Dates are fixed (see [`scene_now`]); the scenes' local site is the same project.
    pub fn web_server() -> FakeRemote {
        let site = |p: &str| format!("/var/www/site/{p}");
        let mut files: Vec<(String, usize, SystemTime)> = vec![
            (site("index.html"), 18_420, ist(17, 16, 44)),
            (site("about.html"), 9_311, ist(18, 16, 12)),
            (site("contact.php"), 4_102, ist(17, 11, 5)),
            (site("old-landing.html"), 7_730, ist(-240, 9, 15)),
            (site("press"), 2_048, ist(-30, 18, 2)),
            // Uploaded a few minutes ago by the job that's now asking about index.html.
            (site("robots.txt"), 68, ist(20, 10, 24)),
            (site("sitemap.xml"), 2_903, ist(20, 10, 24)),
            (site(".htaccess"), 512, ist(-79, 9, 40)),
            (site("favicon.ico"), 15_086, ist(-79, 9, 40)),
            (site("css/site.css"), 34_210, ist(20, 10, 21)),
            (site("js/app.js"), 141_093, ist(18, 15, 58)),
            (site("img/hero.jpg"), 604_330, ist(18, 15, 58)),
            (site("img/logo.svg"), 6_204, ist(18, 15, 58)),
            ("/var/www/logs/access.log".into(), 9_000_000, ist(20, 10, 29)),
            ("/var/www/logs/error.log".into(), 412_004, ist(20, 10, 27)),
            ("/var/www/logs/old/2025.tar".into(), 400_000, ist(-30, 0, 0)),
        ];
        for i in 1..=10 {
            files.push((site(&format!("img/team-{i:02}.jpg")), 60_000, ist(18, 15, 58)));
        }
        let files: Vec<(&str, usize, SystemTime)> = files.iter().map(|(p, n, t)| (p.as_str(), *n, *t)).collect();
        FakeRemote::from_files(&files, &[("/var/www/site", ist(20, 10, 24))])
    }

    /// A Mac's home folder for scenes: the same site as a local project (some of it newer, and
    /// `img` half downloaded), and a client's design folder deep enough to fold the path bar.
    pub fn mac() -> FakeRemote {
        let site = |p: &str| format!("/Users/alex/Projects/acme-site/{p}");
        let design = |p: &str| format!("/Users/alex/Documents/Clients/Acme Corporation/Website Redesign 2026/Final Assets/{p}");
        let mut files: Vec<(String, usize, SystemTime)> = vec![
            (site("index.html"), 18_902, ist(20, 9, 44)),
            (site("about.html"), 9_420, ist(20, 9, 12)),
            (site("contact.php"), 4_215, ist(20, 8, 57)),
            (site("robots.txt"), 68, ist(19, 18, 5)),
            (site("sitemap.xml"), 2_903, ist(20, 9, 40)),
            (site("favicon.ico"), 15_086, ist(-79, 9, 40)),
            (site("README.md"), 1_284, ist(15, 14, 30)),
            (site("release-2026-11.tar.gz"), 48_211_904, ist(20, 10, 2)),
            (site("css/site.css"), 34_210, ist(20, 10, 5)),
            (site("js/app.js"), 141_093, ist(18, 15, 58)),
            (site("press/2026-launch.md"), 3_412, ist(16, 12, 20)),
            // The paused download of `img` got this far.
            (site("img/logo.svg"), 6_204, ist(20, 10, 26)),
            (design("homepage-v3.psd"), 52_430_112, ist(19, 17, 48)),
            (design("hero-banner.png"), 2_843_551, ist(19, 17, 40)),
            (design("logo-mark.svg"), 6_204, ist(18, 11, 2)),
            (design("brand-palette.pdf"), 418_330, ist(12, 15, 30)),
            (design("copy-deck.docx"), 88_041, ist(19, 9, 15)),
            (design("fonts.zip"), 1_902_664, ist(3, 10, 0)),
            (design("photos/team-01.jpg"), 60_000, ist(18, 15, 58)),
            (design("notes.txt"), 2_210, ist(20, 9, 5)),
            ("/Users/alex/Desktop/screenshot.png".into(), 412_993, ist(20, 9, 1)),
        ];
        for i in 1..=4 {
            files.push((site(&format!("img/team-{i:02}.jpg")), 60_000, ist(20, 10, 27)));
        }
        let files: Vec<(&str, usize, SystemTime)> = files.iter().map(|(p, n, t)| (p.as_str(), *n, *t)).collect();
        let mut f = FakeRemote::from_files(&files, &[("/Users/alex/Projects/acme-site/drafts", ist(14, 16, 0))]);
        f.home = Some(RPath::new("/Users/alex"));
        f
    }

    /// A team's cloud drive: a folder someone shared with you (`Design`), one you shared
    /// (`Budget 2027`), and ordinary folders and files.
    pub fn cloud_drive() -> FakeRemote {
        let at = |p: &str| format!("/Acme/{p}");
        let files: Vec<(String, usize, SystemTime)> = vec![
            (at("Design/brand-guide.pdf"), 4_210_332, ist(16, 14, 2)),
            (at("Design/logo-mark.svg"), 6_204, ist(18, 15, 58)),
            (at("Budget 2027/forecast.xlsx"), 88_120, ist(19, 11, 40)),
            (at("Contracts/acme-msa.pdf"), 512_004, ist(2, 9, 30)),
            (at("Photos/offsite-01.jpg"), 2_604_330, ist(12, 17, 15)),
            (at("meeting-notes.md"), 3_120, ist(20, 9, 50)),
            (at("roadmap.key"), 9_811_200, ist(17, 16, 5)),
        ];
        let files: Vec<(&str, usize, SystemTime)> = files.iter().map(|(p, n, t)| (p.as_str(), *n, *t)).collect();
        let mut f = FakeRemote::from_files(&files, &[]);
        f.home = Some(RPath::new("/Acme"));
        f.shared.insert(RPath::new("/Acme/Design"), Shared { owned_by_me: false, can_leave: true });
        f.shared.insert(RPath::new("/Acme/Budget 2027"), Shared { owned_by_me: true, can_leave: false });
        f
    }

    fn entry(&self, path: &RPath, n: &Node) -> Entry {
        Entry {
            name: if path.is_root() { "/".into() } else { path.name().to_owned() },
            path: path.clone(),
            kind: if n.dir { EntryKind::Dir } else { EntryKind::File },
            size: (!n.dir).then_some(n.data.len() as u64),
            modified: Some(n.modified),
            mode: Some(n.mode),
            owner: Some("deploy".into()),
            group: Some("www-data".into()),
            link_target: None,
            extra: Vec::new(), checksums: Vec::new(), shared: self.shared.get(path).copied(),
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<RPath, Node>>> {
        self.nodes.lock().map_err(|_| Error::new(ErrorKind::Other, "fake remote poisoned"))
    }

    fn missing(p: &RPath) -> Error {
        Error::new(ErrorKind::NotFound, format!("“{}” doesn't exist on the server.", p.name()))
    }
}

struct Sink {
    nodes: Arc<Mutex<BTreeMap<RPath, Node>>>,
    path: RPath,
    buf: Vec<u8>,
}

impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl WriteSink for Sink {
    fn finish(self: Box<Self>) -> Result<()> {
        let mut n = self.nodes.lock().map_err(|_| Error::new(ErrorKind::Other, "fake remote poisoned"))?;
        n.insert(self.path.clone(), Node { dir: false, data: self.buf, mode: 0o644, modified: SystemTime::now() });
        Ok(())
    }
}

impl Remote for FakeRemote {
    fn caps(&self) -> Caps {
        Caps { permissions: true, real_dirs: true, server_rename: true, set_mtime: true, ..Caps::default() }
    }
    fn home(&self) -> Result<RPath> {
        Ok(self.home.clone().unwrap_or_else(|| RPath::new("/var/www/site")))
    }
    fn list(&self, dir: &RPath) -> Result<Vec<Entry>> {
        let n = self.lock()?;
        match n.get(dir) {
            Some(d) if d.dir => {}
            Some(_) => return Err(Error::new(ErrorKind::InvalidPath, format!("“{}” isn't a folder.", dir.name()))),
            None => return Err(Self::missing(dir)),
        }
        Ok(n.iter().filter(|(p, _)| p.parent().as_ref() == Some(dir)).map(|(p, node)| self.entry(p, node)).collect())
    }
    fn stat(&self, path: &RPath) -> Result<Entry> {
        let n = self.lock()?;
        n.get(path).map(|node| self.entry(path, node)).ok_or_else(|| Self::missing(path))
    }
    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        let n = self.lock()?;
        let node = n.get(path).ok_or_else(|| Self::missing(path))?;
        Ok(Box::new(Cursor::new(node.data[(offset as usize).min(node.data.len())..].to_vec())))
    }
    fn write(&self, path: &RPath, _offset: u64, _size: Option<u64>) -> Result<Box<dyn WriteSink>> {
        Ok(Box::new(Sink { nodes: self.nodes.clone(), path: path.clone(), buf: Vec::new() }))
    }
    fn mkdir(&self, path: &RPath) -> Result<()> {
        let mut n = self.lock()?;
        if n.contains_key(path) {
            return Err(Error::new(ErrorKind::AlreadyExists, format!("“{}” already exists.", path.name())));
        }
        n.insert(path.clone(), Node { dir: true, data: Vec::new(), mode: 0o755, modified: SystemTime::now() });
        Ok(())
    }
    fn rename(&self, from: &RPath, to: &RPath) -> Result<()> {
        let mut n = self.lock()?;
        if n.contains_key(to) {
            return Err(Error::new(ErrorKind::AlreadyExists, format!("“{}” already exists.", to.name())));
        }
        let moving: Vec<RPath> = n.keys().filter(|p| p.starts_with(from)).cloned().collect();
        if moving.is_empty() {
            return Err(Self::missing(from));
        }
        for p in moving {
            if let Some(node) = n.remove(&p) {
                let rest = &p.as_str()[from.as_str().len()..];
                n.insert(RPath::new(&format!("{}{rest}", to.as_str())), node);
            }
        }
        Ok(())
    }
    fn remove_file(&self, path: &RPath) -> Result<()> {
        self.lock()?.remove(path).map(|_| ()).ok_or_else(|| Self::missing(path))
    }
    fn remove_dir(&self, path: &RPath) -> Result<()> {
        let mut n = self.lock()?;
        if n.keys().any(|p| p.parent().as_ref() == Some(path)) {
            return Err(Error::new(ErrorKind::InvalidPath, format!("“{}” isn't empty.", path.name())));
        }
        n.remove(path).map(|_| ()).ok_or_else(|| Self::missing(path))
    }
    /// Takes a shared item out of this account: it disappears here and stays for everyone else.
    fn leave_shared(&self, path: &RPath) -> Result<()> {
        if !self.shared.get(path).is_some_and(|s| s.can_leave) {
            return Err(Error::unsupported("leaving this item"));
        }
        let mut n = self.lock()?;
        n.retain(|p, _| !p.starts_with(path));
        Ok(())
    }
    fn set_mode(&self, path: &RPath, mode: u32) -> Result<()> {
        let mut n = self.lock()?;
        let node = n.get_mut(path).ok_or_else(|| Self::missing(path))?;
        node.mode = mode;
        Ok(())
    }
}
