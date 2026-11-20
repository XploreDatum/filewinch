//! Live folders. A pane lists again when a transfer writes, moves or deletes something in the
//! folder it shows or below it (the engine's per-file events), or when another app changes the
//! This Mac folder it shows (`crate::watch`). Re-lists are debounced ([`DEBOUNCE`]), quiet
//! (rows, selection, scroll and an open rename stay until the new listing lands) and never more
//! than one at a time per pane, so a 2,000-file job lists a couple of times a second at most.
//!
//! While a job writes into the shown folder its files show as rows saying "Downloading…" (or
//! "Uploading…", "Copying…") with a spinner, even before the listing has them, so the folder
//! never looks empty mid-transfer. A finished file keeps its row until the listing that has it
//! lands, so the row turns into the real one in place.

use std::collections::{HashMap, HashSet};

use filewinch_core::{Entry, EntryKind, RPath};
use rune::Tx;

use super::Pane;
use crate::transfers::engine::{Event, ItemState, JobId};
use crate::transfers::model::Direction;
use crate::ui::Ui;

/// How long a re-list waits for more changes: the first change schedules it, later ones join it.
pub const DEBOUNCE: f64 = 0.4;

/// A file a job is writing (or has just written) on one of this window's sources.
#[derive(Clone, Debug, PartialEq)]
pub struct Flight {
    pub job: JobId,
    pub session: u64,
    pub path: RPath,
    pub size: Option<u64>,
    pub label: &'static str,
    /// Written: the row stays, without its spinner, until a listing of its folder lands.
    pub done: bool,
}

/// One row a pane shows for a transfer: name, size, and the label while it's still running.
pub type Incoming = (String, Option<u64>, Option<&'static str>);

impl Pane {
    /// Drops what the live view knew about the folder being left.
    pub(crate) fn forget_live(&mut self) {
        self.refresh_at = None;
        self.incoming.clear();
        self.synthetic.clear();
    }

    /// Asks for a quiet re-list within [`DEBOUNCE`]; asks before then join the same one.
    pub fn refresh_soon(&mut self, now: f64) {
        if self.src.is_some() && self.refresh_at.is_none() {
            self.refresh_at = Some(now + DEBOUNCE);
        }
    }

    /// Starts a due re-list, unless a listing is still on its way (it waits for that one).
    /// True when it started one.
    pub fn run_due(&mut self, now: f64, show_hidden: bool) -> bool {
        match self.refresh_at {
            Some(t) if t <= now && !self.busy => {
                self.refresh_at = None;
                self.load_with(None, show_hidden, true);
                true
            }
            _ => false,
        }
    }

    /// Shows `rows` (transfers writing into this folder): their names get the transfer's label,
    /// and names the listing doesn't have yet get rows of their own. True when anything changed.
    pub fn set_incoming(&mut self, rows: Vec<Incoming>, show_hidden: bool) -> bool {
        let labels: HashMap<String, &'static str> = rows.iter().filter_map(|(n, _, l)| l.map(|l| (n.clone(), l))).collect();
        let listed: HashSet<&str> = self.entries.iter().filter(|e| !self.synthetic.contains(&e.name)).map(|e| e.name.as_str()).collect();
        let missing: Vec<&Incoming> = rows.iter().filter(|(n, _, _)| !listed.contains(n.as_str())).collect();
        let want: HashSet<String> = missing.iter().map(|(n, _, _)| n.clone()).collect();
        if labels == self.incoming && want == self.synthetic {
            return false;
        }
        let old = std::mem::take(&mut self.synthetic);
        self.entries.retain(|e| !old.contains(&e.name));
        for (name, size, _) in missing {
            self.entries.push(placeholder(self.dir.join(name), *size));
        }
        self.synthetic = want;
        self.incoming = labels;
        self.rearrange(show_hidden);
        true
    }

    /// Whether `name` is a transfer's placeholder row (not on disk under that name yet).
    pub fn is_placeholder(&self, name: &str) -> bool {
        self.synthetic.contains(name)
    }
}

fn placeholder(path: RPath, size: Option<u64>) -> Entry {
    Entry {
        name: path.name().to_owned(),
        path,
        kind: EntryKind::File,
        size,
        modified: None,
        mode: None,
        owner: None,
        group: None,
        link_target: None,
        extra: Vec::new(),
        checksums: Vec::new(),
        shared: None,
    }
}

/// What a running file says in its Kind column.
fn label(d: Direction) -> Option<&'static str> {
    match d {
        Direction::Download => Some("Downloading…"),
        Direction::Upload => Some("Uploading…"),
        Direction::Across => Some("Copying…"),
        Direction::Delete => None,
    }
}

impl Ui {
    /// The engine's per-file events: in-flight rows, and re-lists of the folders they change.
    pub(crate) fn on_item_event(&mut self, ev: &Event) {
        match ev {
            Event::ItemStarted { job, path, size } => {
                let Some(meta) = self.jobs.get(job) else { return };
                let (Some(session), Some(label)) = (meta.writes, label(meta.direction)) else { return };
                self.flights.retain(|f| !(f.job == *job && f.path == *path));
                self.flights.push(Flight { job: *job, session, path: path.clone(), size: *size, label, done: false });
            }
            Event::ItemDone { job, path, from } => {
                for f in self.flights.iter_mut().filter(|f| f.job == *job && f.path == *path) {
                    f.done = true;
                }
                let Some(meta) = self.jobs.get(job) else { return };
                let (writes, reads) = (meta.writes, meta.reads);
                if let Some(s) = writes {
                    self.changed_at(s, path);
                }
                if let (Some(s), Some(from)) = (reads, from) {
                    self.changed_at(s, from);
                }
            }
            Event::ItemsChanged(job) if self.flights.iter().any(|f| f.job == *job && !f.done) => {
                // A skipped file has no ItemDone: its row goes once it's no longer active.
                let engine = &crate::hub::hub().engine;
                let (n, _) = engine.items(*job, ItemState::Active, 0..0);
                let (_, active) = engine.items(*job, ItemState::Active, 0..n);
                for f in self.flights.iter_mut().filter(|f| f.job == *job && !f.done) {
                    if !active.iter().any(|v| !v.is_dir && f.path.as_str().ends_with(&format!("/{}", v.rel))) {
                        f.done = true;
                    }
                }
            }
            Event::State(id, st) if st.is_finished() => {
                for f in self.flights.iter_mut().filter(|f| f.job == *id) {
                    f.done = true;
                }
            }
            _ => {}
        }
    }

    /// `path` on `session` changed: each pane showing its folder, or a folder above it, lists
    /// again soon.
    pub fn changed_at(&mut self, session: u64, path: &RPath) {
        let now = self.now;
        for p in &mut self.panes {
            if p.session() == Some(session) && path != &p.dir && path.starts_with(&p.dir) {
                p.refresh_soon(now);
            }
        }
    }

    /// Before every frame: due re-lists, in-flight rows and folder watches. True when a pane
    /// changed.
    pub(crate) fn live_poll(&mut self, now: f64, tx: &mut Tx) -> bool {
        let hidden = self.prefs.show_hidden;
        let mut changed = false;
        for p in &mut self.panes {
            changed |= p.run_due(now, hidden);
        }
        // A written file's row goes once its folder's listing has landed (with it in), or when
        // no pane shows its folder.
        let panes = &self.panes;
        self.flights.retain(|f| {
            !f.done
                || panes.iter().any(|p| {
                    p.session() == Some(f.session) && f.path.parent().as_ref() == Some(&p.dir) && (p.busy || p.refresh_at.is_some())
                })
        });
        for i in 0..self.panes.len() {
            let pane = &self.panes[i];
            let rows: Vec<Incoming> = match pane.session() {
                Some(s) if !matches!(pane.listing, super::Listing::Failed(_)) => self
                    .flights
                    .iter()
                    .filter(|f| f.session == s && f.path.parent().as_ref() == Some(&pane.dir))
                    .map(|f| (f.path.name().to_owned(), f.size, (!f.done).then_some(f.label)))
                    .collect(),
                _ => Vec::new(),
            };
            changed |= self.panes[i].set_incoming(rows, hidden);
        }
        self.sync_watches();
        if let Some(t) = self.panes.iter().filter_map(|p| p.refresh_at).reduce(f64::min) {
            tx.wake_at(t);
        }
        changed
    }
}
