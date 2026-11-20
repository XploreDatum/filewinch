//! One file pane: where it is (This Mac or a connected site), its history, its listing, the
//! filter, sort and selection, and an in-place rename.

pub mod bar;
pub mod empty;
#[cfg(test)]
mod empty_tests;
pub mod header;
pub mod keys;
pub mod layout;
pub mod model;
#[cfg(test)]
mod tests;
pub mod view;

use std::collections::HashSet;
use std::sync::Arc;

use filewinch_core::{Caps, Entry, Error, ErrorKind, Protocol, RPath, Remote};
use rune::{Latest, TextEdit, Waker};

use model::{Selection, Sort, TypeAhead, arrange};

/// What a pane browses.
#[derive(Clone)]
pub struct Source {
    /// 0 for This Mac, otherwise the connected session's id.
    pub session: u64,
    pub label: String,
    pub protocol: Protocol,
    pub remote: Arc<dyn Remote>,
}

impl Source {
    /// This Mac. Every pane shares one session, so moves on the Mac are renames.
    pub fn local() -> Source {
        static LOCAL: std::sync::OnceLock<Arc<dyn Remote>> = std::sync::OnceLock::new();
        let remote = LOCAL.get_or_init(|| Arc::new(filewinch_local::LocalRemote::new())).clone();
        Source { session: 0, label: crate::theme::ui_os().this_computer().into(), protocol: Protocol::Local, remote }
    }
    pub fn caps(&self) -> Caps {
        self.remote.caps()
    }
    pub fn is_local(&self) -> bool {
        self.session == 0
    }
}

/// A listing that came back from a worker.
pub struct Listed {
    pub session: u64,
    pub dir: RPath,
    pub result: filewinch_core::Result<Vec<Entry>>,
    /// The folder to go back to if `dir` turns out not to be a folder (a link to a file).
    pub link_from: Option<RPath>,
}

#[derive(Clone, Debug)]
pub enum Listing {
    Loading,
    Ready,
    Failed(Error),
}

/// Renaming a row in place.
pub struct Rename {
    pub original: String,
    pub edit: TextEdit,
    pub error: Option<String>,
}

/// What an empty pane's header and status line say.
pub const NOT_CONNECTED: &str = "Not connected";

pub struct Pane {
    pub idx: usize,
    /// `None` while the pane shows nothing yet (the right pane at launch, or after Disconnect):
    /// no listing, no IO, and nothing to act on until it connects or shows This Mac.
    pub src: Option<Source>,
    pub dir: RPath,
    pub back: Vec<RPath>,
    pub forward: Vec<RPath>,
    pub listing: Listing,
    pub entries: Vec<Entry>,
    /// Visible rows: indexes into `entries`, filtered and sorted.
    pub rows: Vec<u32>,
    /// Bumped whenever `rows` changes, for the virtual list.
    pub version: u64,
    pub sort: Sort,
    pub filter: TextEdit,
    /// The path bar while it's being edited.
    pub path_edit: Option<TextEdit>,
    pub sel: Selection,
    pub type_ahead: TypeAhead,
    pub rename: Option<Rename>,
    /// The folder row (or `""` for the pane itself) a drag is over.
    pub drop_target: Option<String>,
    /// A name to select and reveal once the next listing lands (a new folder, a rename).
    pub reveal: Option<String>,
    /// A column header being dragged to a new place.
    pub header_drag: Option<header::HeaderDrag>,
    /// Where the table and its rows are in the window, for drags between panes where the
    /// system drag isn't available (`crate::drag_app`).
    pub rect: [f32; 4],
    pub rows_rect: Option<[f32; 4]>,
    latest: Latest<Listed>,
    /// Lists on the calling thread (headless runs and tests, for determinism).
    sync_io: bool,
}

impl Pane {
    pub fn new(idx: usize, src: Option<Source>, dir: RPath, sync_io: bool) -> Pane {
        Pane {
            idx,
            src,
            dir,
            back: Vec::new(),
            forward: Vec::new(),
            listing: Listing::Loading,
            entries: Vec::new(),
            rows: Vec::new(),
            version: 0,
            sort: Sort::default(),
            filter: TextEdit::new(""),
            path_edit: None,
            sel: Selection::default(),
            type_ahead: TypeAhead::default(),
            rename: None,
            drop_target: None,
            reveal: None,
            header_drag: None,
            rect: [0.0; 4],
            rows_rect: None,
            latest: Latest::default(),
            sync_io,
        }
    }

    pub fn set_waker(&self, waker: Waker) {
        self.latest.set_waker(waker);
    }

    /// True while the pane shows nothing (no source).
    pub fn is_empty(&self) -> bool {
        self.src.is_none()
    }

    pub fn is_local(&self) -> bool {
        self.src.as_ref().is_some_and(Source::is_local)
    }

    /// The session the pane shows, if any (0 for This Mac).
    pub fn session(&self) -> Option<u64> {
        self.src.as_ref().map(|s| s.session)
    }

    /// The source's name, or "Not connected".
    pub fn label(&self) -> &str {
        self.src.as_ref().map_or(NOT_CONNECTED, |s| s.label.as_str())
    }

    /// Shows nothing again: forgets the source, its history, listing, selection and edits. A
    /// listing still in flight is dropped when it lands (its session no longer matches).
    pub fn clear(&mut self, show_hidden: bool) {
        self.src = None;
        self.back.clear();
        self.forward.clear();
        self.entries.clear();
        self.sel = Selection::default();
        self.type_ahead = TypeAhead::default();
        self.rename = None;
        self.path_edit = None;
        self.drop_target = None;
        self.reveal = None;
        self.header_drag = None;
        self.filter = TextEdit::new("");
        self.dir = RPath::root();
        self.listing = Listing::Ready;
        self.rearrange(show_hidden);
    }

    /// Shows `src` at `dir` (a newly connected site, or This Mac again), clearing history.
    pub fn switch_to(&mut self, src: Source, dir: RPath, show_hidden: bool) {
        self.src = Some(src);
        self.back.clear();
        self.forward.clear();
        self.entries.clear();
        self.sel = Selection::default();
        self.filter = TextEdit::new("");
        self.dir = dir;
        self.rearrange(show_hidden);
        self.load(None, show_hidden);
    }

    /// Goes to `dir`, remembering where it was for Back.
    pub fn go(&mut self, dir: RPath, show_hidden: bool) {
        if self.is_empty() {
            return;
        }
        if dir == self.dir {
            return self.load(None, show_hidden);
        }
        self.enter(dir, show_hidden);
        self.load(None, show_hidden);
    }

    /// Opens a symlink as a folder; if it's a link to a file the pane stays where it was.
    pub fn go_link(&mut self, dir: RPath, show_hidden: bool) {
        if self.is_empty() {
            return;
        }
        let from = self.enter(dir, show_hidden);
        self.load(Some(from), show_hidden);
    }

    /// Moves to `dir` and returns where it was.
    fn enter(&mut self, dir: RPath, show_hidden: bool) -> RPath {
        let from = std::mem::replace(&mut self.dir, dir);
        self.back.push(from.clone());
        self.forward.clear();
        self.leave_folder(show_hidden);
        from
    }

    pub fn up(&mut self, show_hidden: bool) {
        if self.is_empty() {
            return;
        }
        if let Some(parent) = self.dir.parent() {
            // Land on the folder we came out of, as Finder does.
            self.reveal = Some(self.dir.name().to_owned());
            self.go(parent, show_hidden);
        }
    }

    pub fn back(&mut self, show_hidden: bool) {
        if let Some(dir) = self.back.pop() {
            let from = std::mem::replace(&mut self.dir, dir);
            self.forward.push(from);
            self.leave_folder(show_hidden);
            self.load(None, show_hidden);
        }
    }

    pub fn forward(&mut self, show_hidden: bool) {
        if let Some(dir) = self.forward.pop() {
            let from = std::mem::replace(&mut self.dir, dir);
            self.back.push(from);
            self.leave_folder(show_hidden);
            self.load(None, show_hidden);
        }
    }

    fn leave_folder(&mut self, show_hidden: bool) {
        self.entries.clear();
        self.sel = Selection::default();
        self.rename = None;
        self.filter = TextEdit::new("");
        self.rearrange(show_hidden);
    }

    pub fn refresh(&mut self, show_hidden: bool) {
        self.load(None, show_hidden);
    }

    fn load(&mut self, link_from: Option<RPath>, show_hidden: bool) {
        let Some(src) = &self.src else {
            // Nothing to list: an empty pane does no IO.
            self.listing = Listing::Ready;
            return;
        };
        self.listing = Listing::Loading;
        let (remote, session, dir) = (src.remote.clone(), src.session, self.dir.clone());
        let work = move |(remote, dir, link_from): (Arc<dyn Remote>, RPath, Option<RPath>)| Listed {
            session,
            result: remote.list(&dir),
            dir,
            link_from,
        };
        if self.sync_io {
            let listed = work((remote, dir, link_from));
            self.apply(listed, show_hidden);
        } else {
            self.latest.request((remote, dir, link_from), work);
        }
    }

    /// Lands a finished listing, if one is waiting. True when something changed.
    pub fn poll(&mut self, show_hidden: bool) -> bool {
        match self.latest.take() {
            Some(listed) => {
                self.apply(listed, show_hidden);
                true
            }
            None => false,
        }
    }

    pub fn is_loading(&self) -> bool {
        matches!(self.listing, Listing::Loading)
    }

    fn apply(&mut self, listed: Listed, show_hidden: bool) {
        if Some(listed.session) != self.session() || listed.dir != self.dir {
            return; // an answer for somewhere we've left
        }
        match listed.result {
            Ok(entries) => {
                self.entries = entries;
                self.listing = Listing::Ready;
                let names: HashSet<&str> = self.entries.iter().map(|e| e.name.as_str()).collect();
                self.sel.retain_listed(&names);
                if let Some(name) = self.reveal.take().filter(|n| names.contains(n.as_str())) {
                    self.sel.only(&name);
                }
                self.rearrange(show_hidden);
            }
            Err(e) if e.kind == ErrorKind::InvalidPath && listed.link_from.is_some() => {
                // A link to a file: stay in the folder it's in.
                let from = listed.link_from.unwrap_or_else(RPath::root);
                self.back.pop();
                self.dir = from;
                self.load(None, show_hidden);
            }
            Err(e) => {
                self.entries.clear();
                self.listing = Listing::Failed(e);
                self.rearrange(show_hidden);
            }
        }
    }

    /// Re-filters and re-sorts after the listing, filter, sort or hidden-files setting changed.
    pub fn rearrange(&mut self, show_hidden: bool) {
        self.rows = arrange(&self.entries, self.sort, &self.filter.text, show_hidden);
        self.version += 1;
    }

    pub fn row_entry(&self, i: usize) -> Option<&Entry> {
        self.rows.get(i).and_then(|&r| self.entries.get(r as usize))
    }

    /// Visible names, top to bottom.
    pub fn order(&self) -> Vec<&str> {
        self.rows.iter().map(|&r| self.entries[r as usize].name.as_str()).collect()
    }

    pub fn entry(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Selected entries in visible order.
    pub fn selected(&self) -> Vec<Entry> {
        self.rows
            .iter()
            .map(|&r| &self.entries[r as usize])
            .filter(|e| self.sel.is_selected(&e.name))
            .cloned()
            .collect()
    }

    pub fn names(&self) -> HashSet<&str> {
        self.entries.iter().map(|e| e.name.as_str()).collect()
    }

    /// The breadcrumb trail: (label, path) from the root down.
    pub fn crumbs(&self) -> Vec<(String, RPath)> {
        let mut out = vec![(if self.is_local() { "Macintosh HD".to_owned() } else { "/".to_owned() }, RPath::root())];
        let mut p = RPath::root();
        for c in self.dir.components() {
            p = p.join(c);
            out.push((c.to_owned(), p.clone()));
        }
        out
    }

    /// A one-line status: "12 items, 3 selected" or "Loading…".
    pub fn status(&self) -> String {
        if self.is_empty() {
            return NOT_CONNECTED.into();
        }
        match &self.listing {
            Listing::Loading => "Loading…".into(),
            Listing::Failed(_) => "Couldn't list this folder".into(),
            Listing::Ready => {
                let n = self.rows.len();
                let hidden = self.entries.len() - n;
                let items = if n == 1 { "1 item".to_owned() } else { format!("{n} items") };
                let sel = self.sel.names.len();
                let mut s = items;
                if sel > 0 {
                    s.push_str(&format!(", {sel} selected"));
                }
                if hidden > 0 && !self.filter.text.trim().is_empty() {
                    s.push_str(&format!(" ({hidden} filtered out)"));
                }
                s
            }
        }
    }
}
