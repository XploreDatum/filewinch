//! The main window's state, and the plumbing every feature shares: the inbox from worker
//! threads, sessions, notices and preferences.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use filewinch_core::{Error, RPath, Remote, Site};
use filewinch_store::{Prefs, Recents, SitesFile};
use rune::machine::Seq;
use rune::{TextEdit, Tx, Waker};

use crate::connect::flow::Conn;
use crate::connect::form::Form;
use crate::edit_watch::Edits;
use crate::hub::{Inbox, hub, sandboxed};
use crate::info::Info;
use crate::pane::{Pane, Source};
use crate::sync_sheet::SyncSheet;
use crate::transfers::engine::{EngineConfig, Event, JobId};
use crate::transfers::model::{JobMeta, Transfers};

/// A connected site.
pub struct Session {
    pub id: u64,
    pub site: Site,
    pub remote: Arc<dyn Remote>,
}

/// The modal sheet on top of the window, if any.
#[allow(clippy::large_enum_variant)] // one value, held in the window's state
pub enum Sheet {
    Connect(Box<Form>),
    Info(Box<Info>),
    Sync(Box<SyncSheet>),
    ConfirmDelete { pane: usize, paths: Vec<RPath>, names: Vec<String> },
    /// A delete whose selection holds shared items: leave them, or (second step) delete their
    /// contents for everyone.
    DeleteShared(Box<crate::delete_sheet::SharedDelete>),
    ConfirmForget { site_id: String, name: String },
    /// Name a new favorites group, or rename one (`Some(id)`).
    Group { id: Option<String>, edit: TextEdit, error: Option<String> },
    License(Box<crate::license_ui::LicenseSheet>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Info,
    Success,
    Warning,
    Error,
}

/// A message at the bottom of the window. Info and success notices go after a few seconds;
/// warnings and errors stay until closed.
pub struct Notice {
    pub id: u64,
    pub tone: Tone,
    pub text: String,
    pub detail: Option<String>,
    pub shown_detail: bool,
    pub until: Option<f64>,
}

/// What a row drag carries, so a drop on the other pane copies from the source directly.
pub struct DragSrc {
    pub pane: usize,
    pub source: Source,
    pub paths: Vec<RPath>,
    /// The files handed to the system (real paths, or staged copies of remote files).
    pub handed: Vec<std::path::PathBuf>,
}

pub struct Ui {
    pub prefs: Prefs,
    pub sites: SitesFile,
    pub recents: Recents,
    pub panes: [Pane; 2],
    pub active: usize,
    pub sessions: Vec<Session>,
    pub sheet: Option<Sheet>,
    pub conn: Conn,
    pub transfers: Transfers,
    pub jobs: HashMap<JobId, JobMeta>,
    pub edits: Edits,
    pub notices: Vec<Notice>,
    pub sidebar_search: TextEdit,
    pub sidebar_selected: Option<String>,
    pub drag: Option<DragSrc>,
    /// A new item to rename in place once its pane's listing shows it.
    pub pending_rename: Option<(usize, String)>,
    pub inbox: Inbox,
    pub seq: Seq,
    /// Seconds on the UI clock, as of the last poll.
    pub now: f64,
    pub system_dark: bool,
    pub sync_io: bool,
    /// The paid licence: unregistered shows the banner (see `license_ui`).
    pub license: filewinch_license::Status,
    /// The mounted disks and shares, for the location menu and pane headers.
    pub volumes: crate::volumes::Volumes,
    /// The pane location menu, while it's open.
    pub source_menu: Option<crate::pane::source_menu::SourceMenu>,
    /// Files this window's jobs are writing, for the panes' live rows (`pane::live`).
    pub flights: Vec<crate::pane::live::Flight>,
    /// The This Mac folders the panes watch for other apps' changes (`watch`).
    pub watches: crate::watch::Watches,
    /// The Inspector panel's state (what it shows, its previews and edits).
    pub inspector: crate::inspector::Inspector,
    next_id: u64,
}

static SESSION_IDS: AtomicU64 = AtomicU64::new(1);

pub fn next_session_id() -> u64 {
    SESSION_IDS.fetch_add(1, Ordering::Relaxed)
}

impl Ui {
    pub fn new() -> Ui {
        let sync_io = sandboxed();
        crate::hub::load_ssh_hosts();
        let h = hub();
        let mut warnings = Vec::new();
        let (prefs, sites, recents) = match &h.store {
            Some(store) => {
                let p = store.load_prefs();
                let s = store.load_sites();
                let r = store.load_recents();
                warnings.extend([p.warning, s.warning, r.warning].into_iter().flatten());
                (p.value.clamped(), s.value, r.value)
            }
            None => (Prefs::default(), SitesFile::default(), Recents::default()),
        };
        h.engine.set_config(engine_config(&prefs));
        let home = Source::local().remote.home().unwrap_or_else(|_| RPath::root());
        let inbox = Inbox::default();
        h.register(&inbox);
        let mut ui = Ui {
            // The right pane starts empty, offering Connect… or This Mac.
            panes: [Pane::new(0, Some(Source::local()), home, sync_io), Pane::new(1, None, RPath::root(), sync_io)],
            prefs,
            sites,
            recents,
            active: 0,
            sessions: Vec::new(),
            sheet: None,
            conn: Conn::Idle,
            transfers: Transfers::default(),
            jobs: HashMap::new(),
            edits: Edits::default(),
            notices: Vec::new(),
            sidebar_search: TextEdit::new(""),
            sidebar_selected: None,
            drag: None,
            pending_rename: None,
            inbox,
            seq: Seq::default(),
            now: 0.0,
            system_dark: false,
            sync_io,
            license: crate::license_ui::initial(),
            volumes: crate::volumes::Volumes::new(sync_io),
            source_menu: None,
            flights: Vec::new(),
            watches: crate::watch::Watches::new(!sync_io),
            inspector: crate::inspector::Inspector::new(sync_io),
            next_id: 1,
        };
        for w in warnings {
            ui.notify(Tone::Warning, w, None);
        }
        if crate::license_ui::take_sheet_request() {
            ui.open_license_sheet();
        }
        let hidden = ui.prefs.show_hidden;
        for p in &mut ui.panes {
            p.refresh(hidden);
        }
        ui.volumes.refresh();
        ui
    }

    pub fn setup(&mut self, waker: Waker) {
        self.inbox.set_waker(waker.clone());
        self.volumes.set_waker(waker.clone());
        self.inspector.set_waker(waker.clone());
        for p in &self.panes {
            p.set_waker(waker.clone());
        }
    }

    /// Before every frame: land worker results and expire notices. True when anything changed.
    pub fn poll(&mut self, now: f64, tx: &mut Tx) -> bool {
        self.now = now;
        let mut changed = false;
        for f in self.inbox.drain() {
            f(self);
            changed = true;
        }
        let hidden = self.prefs.show_hidden;
        let mut lost = false;
        for p in &mut self.panes {
            let was_failed = matches!(p.listing, crate::pane::Listing::Failed(_));
            changed |= p.poll(hidden);
            // A local folder on another volume stopped listing: it may have been unmounted.
            lost |= !was_failed && p.volume.is_some() && p.is_local() && matches!(p.listing, crate::pane::Listing::Failed(_));
        }
        if lost {
            self.volumes.refresh();
        }
        changed |= self.volumes.poll();
        changed |= self.inspector_poll(tx);
        self.track_volumes();
        changed |= self.live_poll(now, tx);
        if let Some((p, name)) = self.pending_rename.clone()
            && !self.panes[p].is_loading()
        {
            self.pending_rename = None;
            if self.panes[p].entry(&name).is_some() {
                self.panes[p].sel.only(&name);
                self.begin_rename(p, tx);
            }
            changed = true;
        }
        let before = self.notices.len();
        self.notices.retain(|n| n.until.is_none_or(|t| t > now));
        changed |= self.notices.len() != before;
        if let Some(next) = self.notices.iter().filter_map(|n| n.until).reduce(f64::min) {
            tx.wake_at(next);
        }
        changed
    }

    pub fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    pub fn notify(&mut self, tone: Tone, text: impl Into<String>, detail: Option<String>) {
        let id = self.next_id();
        let until = matches!(tone, Tone::Info | Tone::Success).then_some(self.now + 6.0);
        self.notices.push(Notice { id, tone, text: text.into(), detail, shown_detail: false, until });
        // Keep the stack short: the oldest go first.
        while self.notices.len() > 4 {
            self.notices.remove(0);
        }
    }

    pub fn notify_error(&mut self, e: &Error) {
        self.notify(Tone::Error, e.message.clone(), e.detail.clone());
    }

    pub fn dismiss_notice(&mut self, id: u64) {
        self.notices.retain(|n| n.id != id);
    }

    pub fn other(&self, pane: usize) -> usize {
        1 - pane
    }

    /// The first pane that shows nothing, if any.
    pub fn empty_pane(&self) -> Option<usize> {
        self.panes.iter().position(Pane::is_empty)
    }

    /// Where a connection started without naming a pane lands: an empty pane if there is one,
    /// else `preferred`.
    pub fn connect_pane(&self, preferred: usize) -> usize {
        if self.panes[preferred].is_empty() { preferred } else { self.empty_pane().unwrap_or(preferred) }
    }

    pub fn session(&self, id: u64) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    /// Refreshes every pane showing `dir` on `session`.
    pub fn refresh_where(&mut self, session: u64, dir: &RPath) {
        let hidden = self.prefs.show_hidden;
        for p in &mut self.panes {
            if p.session() == Some(session) && p.dir == *dir {
                p.refresh(hidden);
            }
        }
    }

    pub fn set_show_hidden(&mut self, on: bool) {
        self.prefs.show_hidden = on;
        for p in &mut self.panes {
            p.rearrange(on);
        }
        self.save_prefs();
    }

    /// Takes new preferences (from the Settings window or another window).
    pub fn apply_prefs(&mut self, prefs: Prefs) {
        let hidden_changed = prefs.show_hidden != self.prefs.show_hidden;
        self.prefs = prefs;
        if hidden_changed {
            let on = self.prefs.show_hidden;
            for p in &mut self.panes {
                p.rearrange(on);
            }
        }
    }

    pub fn save_prefs(&mut self) {
        hub().engine.set_config(engine_config(&self.prefs));
        let prefs = self.prefs.clone();
        if let Some(store) = &hub().store
            && let Err(e) = store.save_prefs(&prefs)
        {
            self.notify_error(&e);
        }
        let _ = crate::settings::push_prefs(&prefs);
    }

    pub fn save_sites(&mut self) {
        if let Some(store) = &hub().store
            && let Err(e) = store.save_sites(&self.sites)
        {
            self.notify_error(&e);
        }
    }

    pub fn save_recents(&mut self) {
        if let Some(store) = &hub().store
            && let Err(e) = store.save_recents(&self.recents)
        {
            self.notify_error(&e);
        }
    }

    pub fn on_engine_event(&mut self, ev: Event) {
        self.on_item_event(&ev);
        let finished = self.transfers.apply(&ev);
        self.follow_items(&ev);
        if let Some((id, ok)) = finished {
            self.job_finished(id, ok);
        }
        if self.transfers.has_question() {
            self.prefs.transfers_open = true;
        }
    }

    pub fn dark(&self) -> bool {
        crate::theme::is_dark(self.prefs.appearance, self.system_dark)
    }
}

impl Default for Ui {
    fn default() -> Self {
        Ui::new()
    }
}

pub fn engine_config(p: &Prefs) -> EngineConfig {
    EngineConfig {
        max_jobs: p.max_jobs as usize,
        max_files_per_job: p.files_per_job as usize,
        segments_per_file: p.segments_per_file as usize,
        retries: p.retries,
        bandwidth_limit: p.bandwidth_limit,
        ..EngineConfig::default()
    }
}
