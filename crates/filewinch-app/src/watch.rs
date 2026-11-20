//! Watching the folder a This Mac pane shows, so files other apps add, change or remove there
//! show up without a manual refresh: FSEvents on macOS, inotify on Linux,
//! ReadDirectoryChangesW on Windows (the `notify` crate). Only the shown folder, not its
//! subfolders; re-armed when the pane moves, dropped when it leaves This Mac. Network shares
//! aren't watched (their notices are unreliable and can stall), and a folder the OS won't
//! watch is simply not watched: refresh still works by hand.
//!
//! Arming and disarming happen off the UI thread. A change asks the pane for a debounced
//! re-list (`Pane::refresh_soon`); a burst of changes posts to the window once.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use notify::event::{AccessKind, AccessMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::hub::Inbox;
use crate::ui::Ui;

/// One pane's watch.
#[derive(Default)]
pub struct DirWatch {
    /// The folder it's armed (or arming) for.
    pub dir: Option<PathBuf>,
    /// Bumped on every re-arm, so a late watcher or change notice for an older folder is dropped.
    generation: u64,
    watcher: Option<RecommendedWatcher>,
}

/// Both panes' watches. Off in headless runs and tests unless a test turns it on.
#[derive(Default)]
pub struct Watches {
    pub enabled: bool,
    pub panes: [DirWatch; 2],
}

impl Watches {
    pub fn new(enabled: bool) -> Watches {
        Watches { enabled, ..Watches::default() }
    }

    /// Whether pane `p` has a live watcher (for tests).
    pub fn is_armed(&self, p: usize) -> bool {
        self.panes[p].watcher.is_some()
    }
}

/// Changes worth a re-list: anything but reads (a write that closes still counts).
fn matters(kind: &EventKind) -> bool {
    match kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        EventKind::Access(_) => false,
        _ => true,
    }
}

fn arm(dir: PathBuf, p: usize, generation: u64, inbox: Inbox) -> notify::Result<RecommendedWatcher> {
    // One post per burst: set while a post is waiting for the window.
    let posted = Arc::new(AtomicBool::new(false));
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        if !matters(&ev.kind) || posted.swap(true, Ordering::AcqRel) {
            return;
        }
        let posted = posted.clone();
        inbox.post(move |ui: &mut Ui| {
            posted.store(false, Ordering::Release);
            if ui.watches.panes[p].generation == generation {
                let now = ui.now;
                ui.panes[p].refresh_soon(now);
            }
        });
    })?;
    w.watch(&dir, RecursiveMode::NonRecursive)?;
    Ok(w)
}

/// Drops a watcher off the UI thread (stopping a stream can wait on the OS).
fn retire(w: Option<RecommendedWatcher>) {
    if let Some(w) = w {
        let _ = std::thread::Builder::new().name("filewinch-unwatch".into()).spawn(move || drop(w));
    }
}

impl Ui {
    /// The folder pane `p` should watch: its This Mac folder, unless that's on a network share
    /// or names no folder (Windows' drive list).
    fn watch_target(&self, p: usize) -> Option<PathBuf> {
        let pane = &self.panes[p];
        if !pane.is_local() {
            return None;
        }
        let network = self.volumes.containing(&pane.dir).is_some_and(|v| v.kind == filewinch_os::volumes::VolumeKind::Network);
        let path = filewinch_local::local_path(&pane.dir);
        (!network && !path.as_os_str().is_empty()).then_some(path)
    }

    /// Re-arms each pane's watch when the folder it should watch changed.
    pub(crate) fn sync_watches(&mut self) {
        if !self.watches.enabled {
            return;
        }
        for p in 0..self.panes.len() {
            let want = self.watch_target(p);
            let w = &mut self.watches.panes[p];
            if w.dir == want {
                continue;
            }
            w.generation += 1;
            w.dir = want.clone();
            retire(w.watcher.take());
            let Some(dir) = want else { continue };
            let generation = w.generation;
            let inbox = self.inbox.clone();
            self.run(move || arm(dir, p, generation, inbox).ok(), move |ui, watcher| {
                if ui.watches.panes[p].generation == generation {
                    ui.watches.panes[p].watcher = watcher;
                } else {
                    retire(watcher);
                }
            });
        }
    }
}
