//! Named states for headless screenshots (`filewinch --snap out.png --scene NAME`), built from
//! an in-memory Mac and web server with fixed dates, shown against a fixed "now" (Friday
//! 20 November 2026, 10:30 IST), so they're the same every run. One story runs through them:
//! the Mac's `acme-site` project is being published to `/var/www/site` on web1.

use std::sync::Arc;
use std::time::Duration;

use filewinch_core::{Auth, Entry, Error, ErrorKind, Protocol, RPath, SecretRef, Site};
use filewinch_store::{Appearance, Favorite, Recent};
use rune::TextEdit;

use crate::connect::flow::{Conn, Target};
use crate::connect::form::{AuthChoice, Form};
use crate::fake::{FakeRemote, scene_now};
use crate::pane::Source;
use crate::sync_sheet::Phase;
use crate::transfers::engine::{ConflictAsk, JobState, Progress, SyncAction, SyncActionKind, SyncConflict, SyncPlan};
use crate::transfers::model::{Direction, Row};
use crate::ui::{Session, Sheet, Ui};

type Scene = (&'static str, fn(&mut Ui));

pub const SCENES: &[Scene] = &[
    ("empty", empty),
    ("empty-right", empty_right),
    ("empty-right-dark", empty_right_dark),
    ("local", local),
    ("local-dark", local_dark),
    ("remote", remote),
    ("connect-sftp", connect_sftp),
    ("connect-s3", connect_s3),
    ("connect-swift", connect_swift),
    ("transfers", transfers),
    ("transfers-dark", transfers_dark),
    ("info", info),
    ("sync", sync),
    ("trust", trust),
    ("failed", failed),
    ("password", password),
    ("remote-live", remote_live),
    ("sync-comparing", sync_comparing),
    ("delete-shared", delete_shared),
    ("delete-shared-everyone", delete_shared_everyone),
    ("banner", banner),
    ("license-sheet", license_sheet),
    ("license-sheet-limit", license_sheet_limit),
];

const SITE: &str = "/Users/alex/Projects/acme-site";
const DESIGN: &str = "/Users/alex/Documents/Clients/Acme Corporation/Website Redesign 2026/Final Assets";

/// This Mac for scenes: an in-memory home folder (scripts can rename and delete in it freely),
/// and the clock pinned so "Today" means the same day every run.
fn mac() -> Source {
    static MAC: std::sync::OnceLock<Arc<dyn filewinch_core::Remote>> = std::sync::OnceLock::new();
    crate::fmt::freeze_now(scene_now());
    let remote = MAC.get_or_init(|| Arc::new(FakeRemote::mac())).clone();
    Source { remote, ..Source::local() }
}

fn site(id: &str, name: &str, p: Protocol, host: &str, user: &str) -> Site {
    let mut s = Site::new(id, name, p);
    s.host = host.into();
    s.user = user.into();
    s.auth = Auth::Password { secret: SecretRef(format!("{id}/password")) };
    s
}

fn favorites(s: &mut Ui) {
    let clients = s.sites.add_folder("Clients");
    s.sites.upsert(Favorite::new(site("web1", "web1 production", Protocol::Sftp, "web1.example.com", "deploy"), Some(clients.clone())));
    s.sites.upsert(Favorite::new(site("stage", "Staging", Protocol::Sftp, "stage.example.com", "deploy"), Some(clients)));
    s.sites.upsert(Favorite::new(site("assets", "Assets bucket", Protocol::S3, "", "AKIAEXAMPLE"), None));
    s.sites.upsert(Favorite::new(site("nas", "Home NAS", Protocol::WebDavs, "nas.local", "me"), None));
    s.sites.upsert(Favorite::new(site("drive", "Team Drive", Protocol::GoogleDrive, "", "me@example.com"), None));
    s.recents.items.push(Recent { site: site("ftp", "Old FTP host", Protocol::Ftp, "ftp.example.org", "anon"), at: 0 });
}

fn show(s: &mut Ui, p: usize, dir: &str) {
    let hidden = s.prefs.show_hidden;
    s.panes[p].switch_to(mac(), RPath::new(dir), hidden);
}

fn pick(s: &mut Ui, p: usize, names: &[&str]) {
    for (i, n) in names.iter().enumerate() {
        if s.panes[p].entry(n).is_some() {
            if i == 0 {
                s.panes[p].sel.only(n);
            } else {
                s.panes[p].sel.names.insert((*n).to_owned());
            }
        }
    }
}

fn empty(s: &mut Ui) {
    s.prefs.appearance = Appearance::Light;
    s.prefs.transfers_open = false;
    show(s, 0, SITE);
    show(s, 1, &format!("{SITE}/drafts"));
}

/// First launch: This Mac on the left, the right pane waiting for a connection.
fn empty_right(s: &mut Ui) {
    s.prefs.appearance = Appearance::Light;
    s.prefs.transfers_open = false;
    favorites(s);
    show(s, 0, SITE);
    let hidden = s.prefs.show_hidden;
    s.panes[1].clear(hidden);
    s.active = 1;
}

fn empty_right_dark(s: &mut Ui) {
    empty_right(s);
    s.prefs.appearance = Appearance::Dark;
}

fn local(s: &mut Ui) {
    s.prefs.appearance = Appearance::Light;
    favorites(s);
    s.sidebar_selected = Some("web1".into());
    show(s, 0, SITE);
    show(s, 1, DESIGN);
    pick(s, 0, &["index.html"]);
    pick(s, 1, &["hero-banner.png", "logo-mark.svg"]);
    s.active = 1;
}

fn local_dark(s: &mut Ui) {
    local(s);
    s.prefs.appearance = Appearance::Dark;
}

fn connect_session(s: &mut Ui, p: usize) {
    let remote: Arc<dyn filewinch_core::Remote> = Arc::new(FakeRemote::web_server());
    let site = site("web1", "web1 production", Protocol::Sftp, "web1.example.com", "deploy");
    s.sessions.push(Session { id: 7, site: site.clone(), remote: remote.clone() });
    let hidden = s.prefs.show_hidden;
    s.panes[p].switch_to(Source { session: 7, label: site.name, protocol: Protocol::Sftp, remote }, RPath::new("/var/www/site"), hidden);
}

fn remote(s: &mut Ui) {
    local(s);
    connect_session(s, 1);
    pick(s, 1, &["index.html"]);
}

fn connect_sftp(s: &mut Ui) {
    local(s);
    let mut f = Form::new(Protocol::Sftp);
    f.name = TextEdit::new("web1 production");
    f.host = TextEdit::new("web1.example.com");
    f.user = TextEdit::new("deploy");
    f.auth = AuthChoice::KeyFile;
    f.key_path = TextEdit::new("~/.ssh/id_ed25519");
    f.path = TextEdit::new("/var/www/site");
    f.suggestions = filewinch_store::parse_ssh_config("Host web1\n  HostName 10.0.0.5\n  User deploy\nHost backup-box\n  User root\n");
    s.sheet = Some(Sheet::Connect(Box::new(f)));
}

fn connect_s3(s: &mut Ui) {
    local(s);
    let mut f = Form::new(Protocol::S3);
    f.name = TextEdit::new("Assets bucket");
    f.region = TextEdit::new("eu-west-1");
    f.user = TextEdit::new("AKIAEXAMPLE");
    f.path = TextEdit::new("assets/2026");
    f.validate();
    s.sheet = Some(Sheet::Connect(Box::new(f)));
}

fn connect_swift(s: &mut Ui) {
    local(s);
    let mut f = Form::new(Protocol::Swift);
    f.name = TextEdit::new("Archive (Swift)");
    f.host = TextEdit::new("https://identity.example.com/v3");
    f.user = TextEdit::new("alex");
    f.project = TextEdit::new("acme-web");
    f.domain = TextEdit::new("Default");
    f.region = TextEdit::new("RegionOne");
    f.path = TextEdit::new("backups/2026");
    f.validate();
    s.sheet = Some(Sheet::Connect(Box::new(f)));
}

/// The entry at `path` as `remote` lists it, so a scene shows the same size and date the panes do.
fn stat(remote: &Arc<dyn filewinch_core::Remote>, path: &str) -> Entry {
    remote.stat(&RPath::new(path)).unwrap_or_else(|_| panic!("scene data lacks {path}"))
}

struct Job {
    done: u64,
    total: u64,
    files: (u64, u64),
    current: Option<&'static str>,
    speed: f64,
}

fn job(id: u64, label: &str, direction: Direction, state: JobState, j: Job) -> Row {
    Row {
        id,
        label: label.into(),
        direction,
        state,
        progress: Progress {
            bytes_done: j.done,
            bytes_total: Some(j.total),
            files_done: j.files.0,
            files_total: Some(j.files.1),
            current: j.current.map(RPath::new),
            bytes_per_sec: j.speed,
            eta: (j.speed > 0.0).then(|| Duration::from_secs(((j.total - j.done) as f64 / j.speed) as u64)),
        },
        failures: Vec::new(),
        expanded: false,
        question: None,
        apply_to_all: false,
        reveal: None,
    }
}

fn transfers(s: &mut Ui) {
    remote(s);
    s.prefs.transfers_open = true;
    let remote = |p: usize| s.panes[p].src.as_ref().map(|src| src.remote.clone()).expect("both panes show a folder");
    let (server, here) = (remote(1), remote(0));
    // index.html, about.html, contact.php, sitemap.xml, robots.txt and css/site.css: the last two
    // are already up (the server lists them as of 10:24 and 10:21), index.html is in question.
    let upload = Job { done: 68 + 2_903, total: 18_902 + 9_420 + 4_215 + 2_903 + 68 + 34_210, files: (2, 6), current: None, speed: 0.0 };
    let mut ask = job(4, "Uploading 6 items to site on web1 production", Direction::Upload, JobState::WaitingForAnswer, upload);
    ask.question = Some(ConflictAsk { src: stat(&here, &format!("{SITE}/index.html")), dst: stat(&server, "/var/www/site/index.html"), can_resume: false });
    let logs = Job { done: 9_000_000, total: 9_812_004, files: (1, 3), current: None, speed: 0.0 };
    let mut failed = job(3, &format!("Downloading “logs” to acme-site on {}", crate::theme::ui_os().this_computer()), Direction::Download, JobState::Failed(Error::new(ErrorKind::Other, "2 of 3 items couldn't be copied.")), logs);
    failed.failures = vec![
        (RPath::new("/var/www/logs/error.log"), Error::new(ErrorKind::PermissionDenied, "The server refused to read “error.log”: permission denied.")),
        (RPath::new("/var/www/logs/old/2025.tar"), Error::new(ErrorKind::NotFound, "“2025.tar” was deleted on the server before it could be copied.")),
    ];
    failed.expanded = true;
    let release = Job { done: 21_400_000, total: 48_211_904, files: (0, 1), current: Some("/var/www/site/release-2026-11.tar.gz"), speed: 3_850_000.0 };
    // logo.svg and team-01…04.jpg are down (the Mac's img folder has them).
    let img = Job { done: 6_204 + 4 * 60_000, total: 1_210_534, files: (5, 12), current: None, speed: 0.0 };
    let css = Job { done: 34_210, total: 34_210, files: (1, 1), current: None, speed: 0.0 };
    let old = Job { done: 0, total: 7_730, files: (0, 1), current: None, speed: 0.0 };
    s.transfers.rows = vec![
        job(1, "Uploading “release-2026-11.tar.gz” to site on web1 production", Direction::Upload, JobState::Running, release),
        job(2, &format!("Downloading “img” to acme-site on {}", crate::theme::ui_os().this_computer()), Direction::Download, JobState::Paused, img),
        failed,
        ask,
        job(5, "Uploading “css” to site on web1 production", Direction::Upload, JobState::Done, css),
        job(6, "Deleting “old-landing.html” from web1 production", Direction::Delete, JobState::Queued, old),
    ];
}

fn transfers_dark(s: &mut Ui) {
    transfers(s);
    s.prefs.appearance = Appearance::Dark;
}

fn info(s: &mut Ui) {
    remote(s);
    s.panes[1].sel.only("contact.php");
    s.open_info(1);
    if let Some(Sheet::Info(i)) = &mut s.sheet {
        i.mode = Some(0o664);
        i.octal = TextEdit::new("664");
    }
}

fn sync(s: &mut Ui) {
    remote(s);
    s.open_sync();
    if let Some(Sheet::Sync(sh)) = &mut s.sheet {
        sh.delete_orphans = true;
        let a = |kind, rel: &str, is_dir, size| SyncAction { kind, rel: rel.into(), is_dir, size };
        let plan = SyncPlan {
            a_dir: sh.left.1.clone(),
            b_dir: sh.right.1.clone(),
            actions: vec![
                a(SyncActionKind::CopyAToB, "index.html", false, Some(18_902)),
                a(SyncActionKind::CopyAToB, "about.html", false, Some(9_420)),
                a(SyncActionKind::CopyAToB, "contact.php", false, Some(4_215)),
                a(SyncActionKind::CopyAToB, "release-2026-11.tar.gz", false, Some(48_211_904)),
                a(SyncActionKind::CopyAToB, "README.md", false, Some(1_284)),
                a(SyncActionKind::CopyAToB, "drafts", true, None),
                a(SyncActionKind::DeleteInB, "old-landing.html", false, None),
            ],
            conflicts: vec![SyncConflict { rel: "press".into(), reason: "A folder on one side, a file on the other".into() }],
        };
        let mut skipped = std::collections::HashSet::new();
        skipped.insert(4);
        sh.phase = Phase::Plan { plan, skipped };
    }
}

fn target() -> Target {
    let mut site = site("web1", "web1 production", Protocol::Sftp, "web1.example.com", "deploy");
    site.port = Some(22);
    Target { site, pane: 1, secrets: Vec::new(), save: false, folder: None, trusted_once: Vec::new() }
}

fn trust(s: &mut Ui) {
    local(s);
    s.conn = Conn::Trust {
        target: target(),
        fingerprint: "SHA256:k4cE0x9tq1Nf3QdZ8bVJm2hX7rLw5sYpUa6eGiKoTnM".into(),
        reason: "This server's host key isn't one you've connected to before.".into(),
    };
}

fn failed(s: &mut Ui) {
    local(s);
    s.conn = Conn::Failed {
        target: target(),
        error: Error::new(ErrorKind::Network, "web1.example.com didn't answer. Check the address and your network, then try again.")
            .with_detail("connect to 203.0.113.10:22: Operation timed out (os error 60)"),
        details: true,
    };
}

fn password(s: &mut Ui) {
    local(s);
    s.conn = Conn::Password { target: target(), edit: TextEdit::new(""), message: "The server refused the password for deploy@web1.example.com.".into() };
}

/// The right pane on a real server described by `FILEWINCH_LIVE_*` (see `live.rs`).
fn remote_live(s: &mut Ui) {
    local(s);
    crate::live::connect_pane(s, 1);
}

/// The right pane on the team's Dropbox, where `Design` is a folder someone shared with Alex.
fn cloud_session(s: &mut Ui, p: usize) {
    let remote: Arc<dyn filewinch_core::Remote> = Arc::new(FakeRemote::cloud_drive());
    let site = site("dropbox", "Acme Dropbox", Protocol::Dropbox, "", "alex@acme.example");
    s.sessions.push(Session { id: 8, site: site.clone(), remote: remote.clone() });
    let hidden = s.prefs.show_hidden;
    s.panes[p].switch_to(Source { session: 8, label: site.name, protocol: Protocol::Dropbox, remote }, RPath::new("/Acme"), hidden);
}

/// Delete on a shared folder: the confirm offers to leave it.
fn delete_shared(s: &mut Ui) {
    local(s);
    s.prefs.transfers_open = false;
    cloud_session(s, 1);
    pick(s, 1, &["Design"]);
    s.active = 1;
    s.ask_delete(1);
}

/// The secondary path: deleting the shared folder's contents for everyone, before the tick.
fn delete_shared_everyone(s: &mut Ui) {
    delete_shared(s);
    if let Some(Sheet::DeleteShared(d)) = &mut s.sheet {
        d.everyone = Some(false);
    }
}

/// A checksum compare under way: the progress line and the Stop button.
fn sync_comparing(s: &mut Ui) {
    sync(s);
    if let Some(Sheet::Sync(sh)) = &mut s.sheet {
        sh.compare = crate::transfers::engine::CompareBy::Checksum;
        let progress = crate::transfers::engine::CompareProgress { entries: 1_204, bytes_hashed: 312_400_000, current: Some("img/hero.jpg".into()) };
        sh.phase = Phase::Comparing(crate::sync_sheet::Comparing { attempt: s.seq.next(), cancel: Default::default(), progress });
    }
}

/// An unregistered copy: the "free to use" banner above the Transfers drawer.
fn banner(s: &mut Ui) {
    transfers(s);
    s.license = filewinch_license::Status::Unregistered;
}

/// Enter Licence Key, a key pasted and activating.
fn license_sheet(s: &mut Ui) {
    local(s);
    s.license = filewinch_license::Status::Unregistered;
    let mut sheet = crate::license_ui::LicenseSheet::new();
    sheet.key = TextEdit::new("FW-1C285B2D-6CE6-4BC7-B8BE-ADB6A7E304DA");
    sheet.phase = crate::license_ui::Phase::Activating;
    s.sheet = Some(Sheet::License(Box::new(sheet)));
}

/// Enter Licence Key after Polar said the key is on as many devices as it allows.
fn license_sheet_limit(s: &mut Ui) {
    license_sheet(s);
    if let Some(Sheet::License(l)) = &mut s.sheet {
        l.phase = crate::license_ui::Phase::Failed(filewinch_license::Failure::LimitReached);
    }
}
