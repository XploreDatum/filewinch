//! The Inspector: a panel at the window's right edge that follows the active pane's selection.
//! One item shows its preview, facts, permissions, checksums and storage details; several show
//! a summary and a bulk permissions edit; nothing selected shows the folder itself.
//!
//! Everything slow runs off the UI thread through latest-wins requests (`rune::Latest`): a new
//! selection supersedes the old one's work, whose answer is dropped, and a cancel flag stops a
//! download or a folder walk already running. Headless runs and tests (`sync_io`) do the same
//! work inline, so scenes are deterministic.

mod follow;
mod load;
mod parts;
mod perms;
#[cfg(any(test, feature = "dev-tools"))]
pub mod scenes;
#[cfg(test)]
mod tests;
mod view;
mod work;

use std::collections::HashSet;
use std::sync::Arc;

use filewinch_core::{Caps, Entry, Error, RPath, Remote, Site};
use rune::{ImageCache, Latest, TextEdit, Waker};

pub use view::{FOCUS_KEY, ICON, attach};

use load::{Cancel, Dates, Fetched, Sized, Tally};
use perms::PermsEdit;

/// Panel widths in points; below [`OVERLAY_BELOW`] of window the panel floats over the panes.
pub const MIN_W: f32 = 240.0;
pub const MAX_W: f32 = 420.0;
pub const OVERLAY_BELOW: f32 = 900.0;

/// The panel's foldable sections; their ids are what the preferences remember.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Preview,
    Info,
    Permissions,
    Checksums,
    Details,
}

impl Section {
    pub fn id(self) -> &'static str {
        match self {
            Section::Preview => "preview",
            Section::Info => "info",
            Section::Permissions => "permissions",
            Section::Checksums => "checksums",
            Section::Details => "details",
        }
    }
    pub fn title(self) -> &'static str {
        match self {
            Section::Preview => "Preview",
            Section::Info => "Info",
            Section::Permissions => "Permissions",
            Section::Checksums => "Checksums",
            Section::Details => "Details",
        }
    }
}

/// Which selection the panel shows: the pane, where it is, and the selected names.
#[derive(Clone, Debug, PartialEq)]
pub struct Key {
    pub pane: usize,
    pub session: u64,
    pub dir: RPath,
    pub names: HashSet<String>,
}

/// What the panel shows, copied from the pane when the selection changes.
#[derive(Clone)]
pub struct Subject {
    pub pane: usize,
    pub session: u64,
    pub local: bool,
    /// The connected site (for URLs); `None` on this computer.
    pub site: Option<Site>,
    pub label: String,
    pub remote: Arc<dyn Remote>,
    pub caps: Caps,
    pub dir: RPath,
    /// The selection in the pane's order; empty means the folder itself.
    pub items: Vec<Entry>,
    /// Rows the folder lists (for the folder's own info).
    pub count: usize,
}

impl Subject {
    /// The modes the permissions editor works on, when every item has one and the backend can
    /// change them; `None` hides the editor.
    pub fn modes(&self, folder: Option<&Entry>) -> Option<Vec<u32>> {
        if !self.caps.permissions {
            return None;
        }
        let items: Vec<&Entry> = if self.items.is_empty() { folder.into_iter().collect() } else { self.items.iter().collect() };
        if items.is_empty() || !items.iter().all(|e| perms::editable(e)) {
            return None;
        }
        Some(items.iter().filter_map(|e| e.mode).collect())
    }
}

/// Something loaded in the background.
pub enum Load<T> {
    Idle,
    Loading,
    Ready(T),
    Failed(Error),
}

impl<T> Load<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Load::Ready(t) => Some(t),
            _ => None,
        }
    }
}

/// The dates `Entry` lacks, and (with nothing selected) the folder's own entry.
#[derive(Clone, Debug, Default)]
pub struct Details {
    pub dates: Dates,
    pub folder: Option<Entry>,
}

/// The preview of a single file.
pub enum Preview {
    /// No preview for this kind of item.
    None,
    Loading { got: Arc<std::sync::atomic::AtomicU64>, total: Option<u64> },
    /// Over 20 MB: shown only after "Download to preview".
    TooLarge,
    Image(rune::image::Image),
    Text { text: String, truncated: bool },
    /// Not text after all.
    Binary,
    Failed(Error),
}

/// A folder's size, or the selection's folders'.
pub enum Size {
    NotAsked,
    /// A local folder too big to size without asking.
    TooMany,
    Counting(Arc<Tally>),
    Done { bytes: u64, files: u64, folders: u64 },
    Failed(Error),
}

/// Renaming the item in the panel.
pub struct Rename {
    pub edit: TextEdit,
    pub error: Option<String>,
    pub busy: bool,
}

pub struct Inspector {
    sync_io: bool,
    waker: Option<Waker>,
    images: Option<ImageCache>,
    pub key: Option<Key>,
    /// The pane's listing version the subject was copied at.
    pub version: u64,
    pub subject: Option<Subject>,
    pub details: Load<Details>,
    pub preview: Preview,
    pub size: Size,
    pub perms: PermsEdit,
    pub rename: Option<Rename>,
    /// Staged permission changes are waiting on "Apply changes?" before the panel follows the
    /// new selection.
    pub prompt: bool,
    /// The window's content width, to decide between docking and floating.
    pub main_w: f32,
    /// The preview image has been seen decoded (its size now lays it out).
    image_seen: bool,
    details_req: Latest<Result<Details, Error>>,
    preview_req: Latest<Result<Fetched, Error>>,
    size_req: Latest<Result<Sized, Error>>,
    preview_cancel: Cancel,
    size_cancel: Cancel,
}

impl Inspector {
    pub fn new(sync_io: bool) -> Inspector {
        Inspector {
            sync_io,
            waker: None,
            images: None,
            key: None,
            version: 0,
            subject: None,
            details: Load::Idle,
            preview: Preview::None,
            size: Size::NotAsked,
            perms: PermsEdit::new(&[]),
            rename: None,
            prompt: false,
            main_w: f32::MAX,
            image_seen: false,
            details_req: Latest::default(),
            preview_req: Latest::default(),
            size_req: Latest::default(),
            preview_cancel: Cancel::default(),
            size_cancel: Cancel::default(),
        }
    }

    pub fn set_waker(&mut self, waker: Waker) {
        self.details_req.set_waker(waker.clone());
        self.preview_req.set_waker(waker.clone());
        self.size_req.set_waker(waker.clone());
        self.images = Some(ImageCache::new(waker.clone()));
        self.waker = Some(waker);
    }

    /// Asks for a poll, so a selection the view noticed first is followed on the next frame.
    pub fn nudge(&self) {
        if let Some(w) = &self.waker {
            w();
        }
    }

    /// Stops every load and walk under way; their answers never land.
    fn cancel_all(&mut self) {
        self.preview_cancel.cancel();
        self.size_cancel.cancel();
        self.details_req.cancel();
        self.preview_req.cancel();
        self.size_req.cancel();
    }

    /// The folder entry the panel describes when nothing is selected.
    pub fn folder(&self) -> Option<&Entry> {
        self.details.ready().and_then(|d| d.folder.as_ref())
    }

    /// The modes the editor works on (see [`Subject::modes`]).
    pub fn modes(&self) -> Option<Vec<u32>> {
        self.subject.as_ref()?.modes(self.folder())
    }

    /// Staged permission changes that would change something.
    pub fn dirty(&self) -> bool {
        self.modes().is_some_and(|m| self.perms.dirty(&m))
    }

    /// Something is counting, downloading or applying, so the panel redraws its progress.
    pub fn busy(&self) -> bool {
        matches!(self.size, Size::Counting(_)) || matches!(self.preview, Preview::Loading { .. }) || self.perms.applying.is_some()
            || matches!(&self.preview, Preview::Image(i) if !i.is_ready())
    }
}

/// A plain-text summary of the panel for the clipboard: name, kind, size, where, dates, mode,
/// owner, group and checksums.
pub fn summary(s: &Subject, details: Option<&Details>, size: &Size, now: std::time::SystemTime) -> String {
    use crate::pane::model::file_kind;
    let mut out = Vec::new();
    let date = |t: Option<std::time::SystemTime>| t.map_or_else(|| "n/a".to_owned(), |t| crate::fmt::date(t, now));
    let folder_size = match size {
        Size::Done { bytes, files, .. } => Some(format!("{} ({}) in {} files", crate::fmt::size(*bytes), crate::fmt::exact_bytes(*bytes), crate::fmt::count(*files))),
        _ => None,
    };
    let one = match s.items.as_slice() {
        [one] => Some(one),
        [] => details.and_then(|d| d.folder.as_ref()),
        _ => None,
    };
    let Some(e) = one else {
        let files: u64 = s.items.iter().filter_map(|e| e.size).sum();
        out.push(format!("{} items", s.items.len()));
        out.push(format!("Files: {}", crate::fmt::size(files)));
        if let Some(sz) = folder_size {
            out.push(format!("Total: {sz}"));
        }
        for e in &s.items {
            out.push(format!("  {}", e.path));
        }
        return out.join("\n");
    };
    out.push(format!("Name: {}", e.name));
    out.push(format!("Kind: {}", file_kind(e).describe(&e.name)));
    match (e.is_dir(), folder_size) {
        (true, Some(sz)) => out.push(format!("Size: {sz}")),
        (true, None) => {}
        (false, _) => out.push(format!("Size: {}", e.size.map_or_else(|| "Unknown".into(), |n| format!("{} ({})", crate::fmt::size(n), crate::fmt::exact_bytes(n))))),
    }
    out.push(format!("Path: {}", e.path));
    out.push(format!("URL: {}", crate::urls::url_for(s.site.as_ref(), &e.path)));
    let dates = details.map(|d| d.dates).unwrap_or_default();
    out.push(format!("Created: {}", date(dates.created)));
    out.push(format!("Modified: {}", date(e.modified)));
    out.push(format!("Accessed: {}", date(dates.accessed)));
    if let Some(m) = e.mode {
        out.push(format!("Permissions: {} ({})", perms::octal(m), crate::fmt::mode_string(m)));
    }
    if let Some(o) = &e.owner {
        out.push(format!("Owner: {o}"));
    }
    if let Some(g) = &e.group {
        out.push(format!("Group: {g}"));
    }
    if let Some(t) = &e.link_target {
        out.push(format!("Points to: {t}"));
    }
    for c in &e.checksums {
        out.push(format!("{}: {}", view::algo_name(c.algo), c.hex));
    }
    for (k, v) in &e.extra {
        out.push(format!("{k}: {v}"));
    }
    out.join("\n")
}
