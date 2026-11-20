//! The disks and shares mounted on this computer, for the pane's location menu and header, read
//! off the UI thread (`filewinch_os::volumes::read` can wait on a share that stopped
//! answering): at launch and each time the menu opens. The menu shows the last list at once and
//! updates when the new one lands.

use filewinch_core::RPath;
use filewinch_os::volumes::{self, Volume};
use rune::{Latest, Waker};

use crate::ui::Ui;

pub struct Volumes {
    /// The last list read, boot disk first.
    pub list: Vec<Volume>,
    /// Scenes and tests: the list stays as given and nothing reads the system.
    fixed: bool,
    latest: Latest<Vec<Volume>>,
}

impl Volumes {
    /// `fixed`: never read the system (headless runs and tests start with no volumes).
    pub fn new(fixed: bool) -> Volumes {
        Volumes { list: Vec::new(), fixed, latest: Latest::default() }
    }

    pub fn set_waker(&self, waker: Waker) {
        self.latest.set_waker(waker);
    }

    /// Reads the list again on a worker; [`Volumes::poll`] lands it.
    pub fn refresh(&mut self) {
        if !self.fixed {
            self.latest.request((), |()| volumes::read());
        }
    }

    /// Lands a fresh list, if one is waiting. True when it differs from the last.
    pub fn poll(&mut self) -> bool {
        match self.latest.take() {
            Some(list) if list != self.list => {
                self.list = list;
                true
            }
            _ => false,
        }
    }

    /// Replaces the list and stops reading the system (scenes and tests).
    pub fn set(&mut self, list: Vec<Volume>) {
        self.fixed = true;
        self.latest.cancel();
        self.list = list;
    }

    /// The volume a local folder is on: the deepest mount that contains it.
    pub fn containing(&self, dir: &RPath) -> Option<&Volume> {
        volumes::containing(&self.list, &filewinch_local::local_path(dir))
    }
}

impl Ui {
    /// The non-boot volume a local pane is inside, per the last list ("CMG-512M2").
    pub fn pane_volume(&self, p: usize) -> Option<&Volume> {
        let pane = &self.panes[p];
        if !pane.is_local() {
            return None;
        }
        self.volumes.containing(&pane.dir).filter(|v| !v.boot)
    }

    /// The volume the pane's header names: the one it's inside, or the one it was inside
    /// while it's still in that volume's folder (after an unmount the list no longer has it).
    pub fn shown_volume(&self, p: usize) -> Option<&Volume> {
        self.pane_volume(p).or_else(|| {
            let pane = &self.panes[p];
            let v = pane.volume.as_ref()?;
            (pane.is_local() && filewinch_local::local_path(&pane.dir).starts_with(&v.path)).then_some(v)
        })
    }

    /// The name of the volume pane `p` was showing when it has been unmounted since: its
    /// listing failed, and the fresh list no longer has it.
    pub fn disconnected(&self, p: usize) -> Option<&str> {
        let pane = &self.panes[p];
        if !matches!(pane.listing, crate::pane::Listing::Failed(_)) {
            return None;
        }
        let v = self.shown_volume(p)?;
        (!self.volumes.list.iter().any(|x| x.path == v.path)).then_some(v.name.as_str())
    }

    /// Remembers which volume each local pane is inside, so an unmount can be named.
    pub(crate) fn track_volumes(&mut self) {
        for p in 0..self.panes.len() {
            let now = self.pane_volume(p).cloned();
            let keep = now.or_else(|| self.shown_volume(p).cloned());
            self.panes[p].volume = keep;
        }
    }

    /// Shows the volume mounted at `path` in pane `p`, from This Mac's own history when the
    /// pane already browses This Mac (so Back returns), else switching it to This Mac.
    pub fn show_volume(&mut self, p: usize, path: &std::path::Path) {
        let dir = filewinch_local::rpath(path);
        let hidden = self.prefs.show_hidden;
        self.active = p;
        if self.panes[p].is_local() {
            self.panes[p].go(dir, hidden);
            return;
        }
        let old = self.panes[p].session();
        self.panes[p].switch_to(crate::pane::Source::local(), dir, hidden);
        if let Some(session) = old.filter(|&s| s != 0) {
            self.close_if_unused(session);
        }
    }

    /// Back to the home folder, after the volume the pane showed went away. Back still works.
    pub fn go_home(&mut self, p: usize) {
        let Some(src) = self.panes[p].src.clone() else { return };
        let home = src.remote.home().unwrap_or_else(|_| RPath::root());
        let hidden = self.prefs.show_hidden;
        self.active = p;
        self.panes[p].go(home, hidden);
    }
}
