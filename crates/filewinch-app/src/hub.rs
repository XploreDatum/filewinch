//! What every window shares: the transfer engine, the secret store, the files on disk, and a
//! way for worker threads to hand results to a window ([`Inbox`]).

use std::sync::{Arc, Mutex, OnceLock, Weak};

use filewinch_license::{Licensing, Polar};
use filewinch_store::{MemorySecrets, SecretStore, Store, SystemSecrets};
use rune::Waker;

use crate::transfers::engine::{Engine, EngineConfig, Event};
use crate::ui::Ui;

type Job = Box<dyn FnOnce(&mut Ui) + Send>;

#[derive(Default)]
struct InboxInner {
    queue: Mutex<Vec<Job>>,
    waker: Mutex<Option<Waker>>,
}

/// Work a background thread hands to one window's state; the window's `poll` applies it on the
/// UI thread. Cloning shares the same queue.
#[derive(Clone, Default)]
pub struct Inbox(Arc<InboxInner>);

impl Inbox {
    pub fn post(&self, f: impl FnOnce(&mut Ui) + Send + 'static) {
        if let Ok(mut q) = self.0.queue.lock() {
            q.push(Box::new(f));
        }
        if let Some(w) = self.0.waker.lock().ok().and_then(|w| w.clone()) {
            w();
        }
    }

    pub fn set_waker(&self, waker: Waker) {
        if let Ok(mut w) = self.0.waker.lock() {
            *w = Some(waker);
        }
    }

    pub fn drain(&self) -> Vec<Job> {
        self.0.queue.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
    }
}

pub struct Hub {
    pub engine: Engine,
    pub secrets: Arc<dyn SecretStore>,
    /// `None` in headless runs and tests, which never touch the person's files.
    pub store: Option<Store>,
    /// This device's paid licence. Headless runs and tests keep it in memory and never reach
    /// Polar (their client points at a closed local port).
    pub licensing: Licensing,
    windows: Mutex<Vec<Weak<InboxInner>>>,
}

static HUB: OnceLock<Hub> = OnceLock::new();

/// Whether this process renders headless (agent tooling) or runs tests: nothing touches the
/// system secret store or the person's saved files then.
pub fn sandboxed() -> bool {
    cfg!(test) || rune::tooling::headless_run().is_some() || std::env::var_os("FILEWINCH_SANDBOX").is_some()
}

pub fn hub() -> &'static Hub {
    HUB.get_or_init(|| {
        let (secrets, store): (Arc<dyn SecretStore>, Option<Store>) = if sandboxed() {
            (Arc::new(MemorySecrets::default()), None)
        } else {
            (Arc::new(SystemSecrets), Store::default_dir().map(Store::new))
        };
        let licensing = if sandboxed() {
            let offline = Polar::new("http://127.0.0.1:9", filewinch_license::ORGANIZATION_ID, filewinch_license::BENEFIT_ID);
            Licensing::new(offline, secrets.clone(), None)
        } else {
            Licensing::new(Polar::default(), secrets.clone(), store.as_ref().map(|s| s.dir().to_path_buf()))
        };
        Hub {
            engine: Engine::new(EngineConfig::default(), broadcast_event),
            secrets,
            store,
            licensing,
            windows: Mutex::new(Vec::new()),
        }
    })
}

fn broadcast_event(ev: Event) {
    let Some(h) = HUB.get() else { return };
    h.each(|inbox| {
        let ev = ev.clone();
        inbox.post(move |ui| ui.on_engine_event(ev));
    });
}

impl Hub {
    pub fn register(&self, inbox: &Inbox) {
        if let Ok(mut w) = self.windows.lock() {
            w.retain(|x| x.strong_count() > 0);
            w.push(Arc::downgrade(&inbox.0));
        }
    }

    /// The first open window's inbox (the one a request from Settings goes to).
    pub fn first(&self) -> Option<Inbox> {
        self.windows.lock().ok()?.iter().find_map(Weak::upgrade).map(Inbox)
    }

    /// Runs `f` for every open window's inbox.
    pub fn each(&self, f: impl Fn(&Inbox)) {
        let live: Vec<Inbox> = match self.windows.lock() {
            Ok(w) => w.iter().filter_map(Weak::upgrade).map(Inbox).collect(),
            Err(_) => return,
        };
        for inbox in &live {
            f(inbox);
        }
    }
}

static SSH_HOSTS: OnceLock<Vec<filewinch_store::SshHost>> = OnceLock::new();

/// Reads `~/.ssh/config` once, off the UI thread (call early; [`ssh_hosts`] is empty until it
/// lands). Headless runs and tests never read it.
pub fn load_ssh_hosts() {
    if sandboxed() {
        let _ = SSH_HOSTS.set(Vec::new());
        return;
    }
    if SSH_HOSTS.get().is_some() {
        return;
    }
    std::thread::Builder::new()
        .name("filewinch-ssh-config".into())
        .spawn(|| {
            let _ = SSH_HOSTS.set(filewinch_store::read_ssh_config());
        })
        .ok();
}

/// Hosts from `~/.ssh/config`, for the connect sheet's suggestions.
pub fn ssh_hosts() -> Vec<filewinch_store::SshHost> {
    SSH_HOSTS.get().cloned().unwrap_or_default()
}
