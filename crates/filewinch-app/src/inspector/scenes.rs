//! Headless scenes for the Inspector (`filewinch --snap out.png --scene inspector-file`), built
//! on the app's own scenes (`crate::scenes`) with real file contents laid over the in-memory
//! trees: an HTML page to show as text, a PNG to show as a picture, and server checksums.

use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::sync::Arc;

use filewinch_core::{Caps, Checksum, Entry, HashAlgo, RPath, Remote, Result, WriteSink};

use crate::pane::Source;
use crate::ui::Ui;

const SITE: &str = "/Users/alex/Projects/acme-site";
const DESIGN: &str = "/Users/alex/Documents/Clients/Acme Corporation/Website Redesign 2026/Final Assets";

const PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>Acme — Tools for makers</title>
  <link rel="stylesheet" href="/css/site.css">
</head>
<body>
  <header class="hero">
    <img src="/img/logo.svg" alt="Acme">
    <h1>Tools for makers</h1>
    <p>Built in Pune, shipped everywhere.</p>
  </header>
  <main>
    <section id="team">…</section>
  </main>
  <script src="/js/app.js" defer></script>
</body>
</html>
"#;

/// A remote that serves `inner`'s tree with some files' bytes, checksums and storage details
/// replaced, as a real server would report them.
struct Dressed {
    inner: Arc<dyn Remote>,
    bytes: HashMap<RPath, Vec<u8>>,
    checksums: HashMap<RPath, Vec<Checksum>>,
}

impl Dressed {
    fn dress(&self, mut e: Entry) -> Entry {
        if let Some(b) = self.bytes.get(&e.path) {
            e.size = Some(b.len() as u64);
        }
        if let Some(c) = self.checksums.get(&e.path) {
            e.checksums = c.clone();
        }
        e
    }
}

impl Remote for Dressed {
    fn caps(&self) -> Caps {
        self.inner.caps()
    }
    fn home(&self) -> Result<RPath> {
        self.inner.home()
    }
    fn list(&self, dir: &RPath) -> Result<Vec<Entry>> {
        Ok(self.inner.list(dir)?.into_iter().map(|e| self.dress(e)).collect())
    }
    fn stat(&self, path: &RPath) -> Result<Entry> {
        self.inner.stat(path).map(|e| self.dress(e))
    }
    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        match self.bytes.get(path) {
            Some(b) => Ok(Box::new(Cursor::new(b[(offset as usize).min(b.len())..].to_vec()))),
            None => self.inner.read(path, offset),
        }
    }
    fn write(&self, path: &RPath, offset: u64, size: Option<u64>) -> Result<Box<dyn WriteSink>> {
        self.inner.write(path, offset, size)
    }
    fn mkdir(&self, path: &RPath) -> Result<()> {
        self.inner.mkdir(path)
    }
    fn rename(&self, from: &RPath, to: &RPath) -> Result<()> {
        self.inner.rename(from, to)
    }
    fn remove_file(&self, path: &RPath) -> Result<()> {
        self.inner.remove_file(path)
    }
    fn remove_dir(&self, path: &RPath) -> Result<()> {
        self.inner.remove_dir(path)
    }
    fn set_mode(&self, path: &RPath, mode: u32) -> Result<()> {
        self.inner.set_mode(path, mode)
    }
}

/// Runs one of the app's scenes by name.
fn base(s: &mut Ui, name: &str) {
    if let Some((_, f)) = crate::scenes::SCENES.iter().find(|(n, _)| *n == name) {
        f(s);
    }
}

/// Pane `p` again, on the same session, through a [`Dressed`] copy of its remote.
fn dress(s: &mut Ui, p: usize, bytes: Vec<(String, Vec<u8>)>, checksums: Vec<(String, Vec<Checksum>)>) {
    let Some(src) = s.panes[p].src.clone() else { return };
    let remote: Arc<dyn Remote> = Arc::new(Dressed {
        inner: src.remote.clone(),
        bytes: bytes.into_iter().map(|(p, b)| (RPath::new(&p), b)).collect(),
        checksums: checksums.into_iter().map(|(p, c)| (RPath::new(&p), c)).collect(),
    });
    if let Some(sess) = s.sessions.iter_mut().find(|x| x.id == src.session) {
        sess.remote = remote.clone();
    }
    let dir = s.panes[p].dir.clone();
    let hidden = s.prefs.show_hidden;
    s.panes[p].switch_to(Source { remote, ..src }, dir, hidden);
}

fn pick(s: &mut Ui, p: usize, names: &[&str]) {
    s.panes[p].sel.names.clear();
    for n in names {
        s.panes[p].sel.names.insert((*n).to_owned());
    }
    s.panes[p].sel.cursor = names.first().map(|n| (*n).to_owned());
    s.active = p;
}

fn open(s: &mut Ui) {
    s.prefs.transfers_open = false;
    s.prefs.inspector_open = true;
    s.inspector_follow();
}

/// A local HTML page: its facts and the first of its text.
pub fn file(s: &mut Ui) {
    base(s, "local");
    dress(s, 0, vec![(format!("{SITE}/index.html"), PAGE.as_bytes().to_vec())], Vec::new());
    pick(s, 0, &["index.html"]);
    open(s);
}

/// A picture from the design folder, previewed.
pub fn image(s: &mut Ui) {
    base(s, "local");
    dress(s, 1, vec![(format!("{DESIGN}/hero-banner.png"), banner_png(480, 270))], Vec::new());
    pick(s, 1, &["hero-banner.png"]);
    open(s);
}

/// Five items: files, two folders, the kinds and the bulk permissions edit.
pub fn multi(s: &mut Ui) {
    base(s, "local");
    pick(s, 0, &["index.html", "about.html", "contact.php", "css", "img"]);
    open(s);
}

/// index.html on web1 over SFTP: the server's checksums, its mode, owner and group, its URL.
pub fn remote(s: &mut Ui) {
    base(s, "remote");
    let path = "/var/www/site/index.html".to_owned();
    let sums: Vec<Checksum> = [
        Checksum::new(HashAlgo::Sha256, "9f2c4be1a07d3e58c6b1f04d2e7a9c38b5d61f0e4a2c7b9d8e3f1a6c5b4d2e07"),
        Checksum::new(HashAlgo::Md5, "4b1d9e0c7a2f63e85d1c0b9a8f7e6d5c"),
    ]
    .into_iter()
    .flatten()
    .collect();
    dress(s, 1, vec![(path.clone(), PAGE.as_bytes().to_vec())], vec![(path, sums)]);
    pick(s, 1, &["index.html"]);
    open(s);
}

/// Folds the preview and the facts, as someone editing permissions would, so the editor shows.
fn perms_only(s: &mut Ui) {
    s.prefs.inspector_collapsed = vec!["preview".into(), "info".into()];
}

/// contact.php with changes staged (group may write, world may not read): Apply is ready.
pub fn perms_staged(s: &mut Ui) {
    base(s, "remote");
    perms_only(s);
    pick(s, 1, &["contact.php"]);
    open(s);
    s.toggle_perm_bit(0o020);
    s.toggle_perm_bit(0o004);
}

/// The css folder with a change staged: Apply to Enclosed and its execute option.
pub fn folder_enclosed(s: &mut Ui) {
    base(s, "remote");
    perms_only(s);
    pick(s, 1, &["css"]);
    open(s);
    s.toggle_perm_bit(0o002);
}

/// A PNG banner (sky, sun and hills), stored without compression, so the scenes need no image
/// encoder.
pub fn banner_png(w: u32, h: u32) -> Vec<u8> {
    let mut raw = Vec::with_capacity(((w * 3 + 1) * h) as usize);
    for y in 0..h {
        raw.push(0); // no filter
        for x in 0..w {
            let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
            let hill = 0.72 + 0.08 * (fx * 7.0).sin() + 0.04 * (fx * 17.0 + 1.0).sin();
            let (dx, dy) = (fx - 0.7, (fy - 0.38) * h as f32 / w as f32);
            let px = if fy > hill {
                [(46.0 + 40.0 * fy) as u8, (120.0 + 40.0 * (1.0 - fy)) as u8, 88]
            } else if (dx * dx + dy * dy).sqrt() < 0.08 {
                [255, 214, 102]
            } else {
                [(255.0 - 120.0 * fy) as u8, (150.0 + 60.0 * fy) as u8, (110.0 + 140.0 * fy) as u8]
            };
            raw.extend_from_slice(&px);
        }
    }
    let mut z = vec![0x78, 0x01];
    let chunks: Vec<&[u8]> = raw.chunks(65_535).collect();
    for (i, c) in chunks.iter().enumerate() {
        z.push(u8::from(i + 1 == chunks.len()));
        let n = c.len() as u16;
        z.extend_from_slice(&n.to_le_bytes());
        z.extend_from_slice(&(!n).to_le_bytes());
        z.extend_from_slice(c);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for byte in &raw {
        a = (a + u32::from(*byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    for (kind, data) in [(b"IHDR", ihdr), (b"IDAT", z), (b"IEND", Vec::new())] {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(&data);
        png.extend_from_slice(&body);
        png.extend_from_slice(&crc32(&body).to_be_bytes());
    }
    png
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for b in bytes {
        crc ^= u32::from(*b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}
