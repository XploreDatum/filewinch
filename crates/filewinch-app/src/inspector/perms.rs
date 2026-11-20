//! The Inspector's permissions editor. Changes are staged as bits to turn on and bits to turn
//! off, so one edit applies to several items whose modes differ, then applied to the selection
//! (Apply: `set_mode` on each, in the background with progress and Stop) or to everything inside
//! the selected folders as well (Apply to Enclosed: the engine's `Op::SetMode` job, in the
//! Transfers drawer). Get Info keeps its own direct editor (`info.rs`); both read and write modes
//! with `fmt::mode_string` and `fmt::parse_octal`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use filewinch_core::{Entry, EntryKind, Error, RPath, Remote};
use rune::TextEdit;

use super::load::{Cancel, retrying};
use crate::fmt;
use crate::transfers::engine::{Op, applied_mode};

/// Every permission bit the editor shows: special (setuid, setgid, sticky), then user, group
/// and world read, write and execute.
pub const ALL: u32 = 0o7777;

/// A pending change: bits to turn on and bits to turn off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Staged {
    pub set: u32,
    pub clear: u32,
}

/// One permission bit across the items being edited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tri {
    On,
    Off,
    Mixed,
}

impl Staged {
    pub fn is_empty(self) -> bool {
        self.set == 0 && self.clear == 0
    }

    /// `orig` with the change made (permission bits only).
    pub fn on(self, orig: u32) -> u32 {
        ((orig & ALL) | self.set) & !self.clear & ALL
    }

    /// The engine's `Op::SetMode` for this change: `mode` holds the bits to turn on, `mask`
    /// every bit the person changed (the rest stay as each item has them).
    pub fn op(self, folders_x: bool) -> Op {
        Op::SetMode { mode: self.set, mask: self.set | self.clear, dirs_get_x_where_r: folders_x }
    }

    /// The mode Apply to Enclosed gives an item (the engine's own rule). With `folders_x`,
    /// folders also get execute wherever the change leaves read allowed, so they stay openable.
    pub fn enclosed(self, orig: u32, is_dir: bool, folders_x: bool) -> u32 {
        applied_mode(orig, is_dir, self.set, self.set | self.clear, folders_x)
    }

    /// Flips `bit`: on everywhere if it's off or mixed now, else off everywhere.
    pub fn toggle(&mut self, bit: u32, now: Tri) {
        if now == Tri::On {
            self.set &= !bit;
            self.clear |= bit;
        } else {
            self.clear &= !bit;
            self.set |= bit;
        }
    }

    /// Every item to exactly `mode`.
    pub fn exact(mode: u32) -> Staged {
        Staged { set: mode & ALL, clear: !mode & ALL }
    }
}

/// `bit` across `modes` after `staged`.
pub fn tri(modes: &[u32], staged: Staged, bit: u32) -> Tri {
    let on = modes.iter().filter(|m| staged.on(**m) & bit != 0).count();
    match on {
        0 => Tri::Off,
        n if n == modes.len() => Tri::On,
        _ => Tri::Mixed,
    }
}

/// The one mode every item will have, if they agree.
pub fn uniform(modes: &[u32], staged: Staged) -> Option<u32> {
    let first = staged.on(*modes.first()?);
    modes.iter().all(|m| staged.on(*m) == first).then_some(first)
}

/// "0770".
pub fn octal(mode: u32) -> String {
    format!("{:04o}", mode & ALL)
}

/// Progress of an Apply, counted on the worker.
#[derive(Default, Debug)]
pub struct Tally {
    pub done: AtomicU64,
    pub failed: AtomicU64,
}

/// An Apply under way.
pub struct Applying {
    pub cancel: Cancel,
    pub tally: Arc<Tally>,
}

/// What a finished Apply did.
#[derive(Debug, Default)]
pub struct Applied {
    pub changed: u64,
    pub failures: Vec<(RPath, Error)>,
    pub cancelled: bool,
}

/// Asked before an Apply that reaches more than one item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confirm {
    Bulk,
    Enclosed,
}

/// The editor's state for the inspected items.
pub struct PermsEdit {
    pub staged: Staged,
    pub octal: TextEdit,
    pub octal_error: Option<String>,
    /// Apply to Enclosed: folders get execute where the change leaves read allowed.
    pub folders_x: bool,
    pub applying: Option<Applying>,
    pub confirm: Option<Confirm>,
    /// The last Apply's outcome: a failure summary (with Retry), or what was done.
    pub error: Option<String>,
    pub note: Option<String>,
}

impl PermsEdit {
    pub fn new(modes: &[u32]) -> PermsEdit {
        PermsEdit {
            staged: Staged::default(),
            octal: TextEdit::new(uniform(modes, Staged::default()).map(octal).unwrap_or_default()),
            octal_error: None,
            folders_x: true,
            applying: None,
            confirm: None,
            error: None,
            note: None,
        }
    }

    /// Something is staged that would change at least one item.
    pub fn dirty(&self, modes: &[u32]) -> bool {
        modes.iter().any(|m| self.staged.on(*m) != m & ALL)
    }

    /// The octal field after a toggle or a revert: the one mode, or empty when items differ.
    pub fn sync_octal(&mut self, modes: &[u32]) {
        self.octal = TextEdit::new(uniform(modes, self.staged).map(octal).unwrap_or_default());
        self.octal_error = None;
    }

    pub fn toggle(&mut self, modes: &[u32], bit: u32) {
        self.staged.toggle(bit, tri(modes, self.staged, bit));
        self.sync_octal(modes);
        self.note = None;
    }

    /// The octal field changed: stage it, or say why it can't be used.
    pub fn typed(&mut self) {
        let t = self.octal.text.trim();
        match fmt::parse_octal(t).filter(|_| (3..=4).contains(&t.len())) {
            Some(m) => {
                self.staged = Staged::exact(m);
                self.octal_error = None;
            }
            None => self.octal_error = Some("Use three or four digits from 0 to 7, like 0644.".into()),
        }
        self.note = None;
    }

    pub fn revert(&mut self, modes: &[u32]) {
        self.staged = Staged::default();
        self.sync_octal(modes);
        self.error = None;
        self.confirm = None;
    }
}

/// Every item the editor can change: a file or folder (not a link, whose mode is its target's)
/// with a mode.
pub fn editable(e: &Entry) -> bool {
    e.mode.is_some() && e.kind != EntryKind::Symlink
}

/// Sets `staged` on `items` (one `set_mode` each, skipping items it doesn't change).
pub fn apply(remote: &dyn Remote, items: &[(RPath, u32)], staged: Staged, cancel: &Cancel, tally: &Tally) -> Applied {
    let mut out = Applied::default();
    for (path, orig) in items {
        if cancel.is_cancelled() {
            out.cancelled = true;
            break;
        }
        set(remote, path, *orig, staged.on(*orig), cancel, tally, &mut out);
    }
    out
}

fn set(remote: &dyn Remote, path: &RPath, orig: u32, new: u32, cancel: &Cancel, tally: &Tally, out: &mut Applied) {
    if new == orig & ALL {
        tally.done.fetch_add(1, Ordering::Relaxed);
        return;
    }
    match retrying(cancel, || remote.set_mode(path, new)) {
        Ok(()) => out.changed += 1,
        Err(e) => {
            tally.failed.fetch_add(1, Ordering::Relaxed);
            out.failures.push((path.clone(), e));
        }
    }
    tally.done.fetch_add(1, Ordering::Relaxed);
}

/// "Changed 120 items." / "3 items couldn't be changed: …" for the panel.
pub fn outcome(a: &Applied) -> (Option<String>, Option<String>) {
    let items = |n: u64| if n == 1 { "1 item".to_owned() } else { format!("{} items", fmt::count(n)) };
    let done = if a.cancelled { format!("Stopped after changing {}.", items(a.changed)) } else { format!("Changed {}.", items(a.changed)) };
    match a.failures.as_slice() {
        [] => (None, Some(done)),
        [(p, e)] => (Some(format!("Couldn't change “{}”: {}", p.name(), e.message)), Some(done)),
        [(p, e), rest @ ..] => {
            (Some(format!("{} couldn't be changed. First, “{}”: {}", items(rest.len() as u64 + 1), p.name(), e.message)), Some(done))
        }
    }
}
