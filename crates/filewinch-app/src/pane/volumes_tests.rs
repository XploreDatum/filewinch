//! The location menu's volumes, the header inside a volume, and a volume that goes away, with
//! a fake volume list over an in-memory Mac.

use std::sync::Arc;

use filewinch_core::{RPath, Remote};
use filewinch_os::volumes::{Volume, VolumeKind};
use rune::{Env, Runtime};

use crate::fake::{FakeRemote, ist};
use crate::pane::{Listing, Source};
use crate::ui::Ui;

// This computer's paths as the local backend's RPaths: a Mac (and Linux) mounts volumes under
// folders, Windows gives each a drive. The volumes' own paths come from `local_path`, so the
// same tests hold on every OS.
#[cfg(not(windows))]
mod at {
    pub const BOOT: &str = "/";
    pub const HOME: &str = "/Users/me";
    pub const SSD: &str = "/Volumes/CMG-512M2";
    pub const SHARE: &str = "/Volumes/Studio Share";
    pub const IMAGE: &str = "/Volumes/Installer";
}
#[cfg(windows)]
mod at {
    pub const BOOT: &str = "/C:";
    pub const HOME: &str = "/C:/Users/me";
    pub const SSD: &str = "/D:";
    pub const SHARE: &str = "/Z:";
    pub const IMAGE: &str = "/E:";
}
use at::{BOOT, HOME, IMAGE, SHARE, SSD};

/// The disk path for an RPath on this computer.
fn disk(p: &str) -> std::path::PathBuf {
    filewinch_local::local_path(&RPath::new(p))
}

fn vol(name: &str, path: &str, kind: VolumeKind, boot: bool, free: u64) -> Volume {
    Volume { name: name.into(), path: disk(path), kind, boot, total: Some(free * 2), free: Some(free) }
}

fn volumes() -> Vec<Volume> {
    vec![
        vol("Macintosh HD", BOOT, VolumeKind::Internal, true, 212_000_000_000),
        vol("CMG-512M2", SSD, VolumeKind::External, false, 340_000_000_000),
        vol("Studio Share", SHARE, VolumeKind::Network, false, 2_610_000_000_000),
        vol("Installer", IMAGE, VolumeKind::DiskImage, false, 1_200_000),
    ]
}

/// A Mac with a home folder and an SSD, shown in both panes at the home folder.
fn mac() -> (Ui, Arc<dyn Remote>) {
    let at = ist(20, 9, 0);
    let files = [format!("{HOME}/notes.txt"), format!("{SSD}/Photos/a.jpg"), format!("{SSD}/b.txt"), format!("{SHARE}/brief.pdf")];
    let files: Vec<(&str, usize, std::time::SystemTime)> = files.iter().map(|f| (f.as_str(), 10, at)).collect();
    let remote: Arc<dyn Remote> = Arc::new(FakeRemote::from_files(&files, &[]).with_home(HOME));
    let mut ui = Ui::new();
    ui.volumes.set(volumes());
    for p in 0..2 {
        ui.panes[p].switch_to(Source { remote: remote.clone(), ..Source::local() }, RPath::new(HOME), false);
    }
    (ui, remote)
}

fn runtime(ui: Ui) -> Runtime<Ui> {
    let mut rt = Runtime::new(ui, crate::view::view, (1320.0, 820.0), Env::new(false, 1.0));
    rt.frame(0.0);
    rt
}

/// A frame as the app runs it: `poll` lands workers' results and tracks volumes first.
fn step(rt: &mut Runtime<Ui>) {
    for _ in 0..3 {
        rt.update(|s, tx| {
            s.poll(0.0, tx);
        });
        rt.frame(1.0 / 60.0);
        for f in rt.state.inbox.drain() {
            f(&mut rt.state);
        }
    }
}

fn press_at(rt: &mut Runtime<Ui>, (x, y): (f32, f32)) {
    rt.pointer_move(x, y);
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    step(rt);
}

fn click(rt: &mut Runtime<Ui>, key: &str) {
    let at = rt.find(key).unwrap_or_else(|| panic!("{key} is on screen"));
    press_at(rt, at);
}

/// The live element with this accessible name and role.
fn labelled(rt: &Runtime<Ui>, label: &str, role: &str) -> Option<rune::runtime::ElementInfo> {
    rt.elements().into_iter().find(|e| !e.exiting && e.role == role && e.label.as_deref() == Some(label))
}

fn choose(rt: &mut Runtime<Ui>, label: &str) {
    let e = labelled(rt, label, "menuitemcheckbox").unwrap_or_else(|| panic!("{label} is in the menu"));
    press_at(rt, (e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0));
}

/// The menu item screen readers hear as `name`: its accessibility node.
fn a11y_item(rt: &Runtime<Ui>, name: &str) -> Option<serde_json::Value> {
    fn walk(v: &serde_json::Value, name: &str) -> Option<serde_json::Value> {
        let role = v["role"].as_str().unwrap_or("").replace('-', "").to_ascii_lowercase();
        if role.contains("menuitem") && v["name"].as_str() == Some(name) {
            return Some(v.clone());
        }
        v["children"].as_array()?.iter().find_map(|c| walk(c, name))
    }
    walk(&rt.accessibility_json(), name)
}

/// Whether the menu item `name` carries the tick.
fn ticked(rt: &Runtime<Ui>, name: &str) -> bool {
    let item = a11y_item(rt, name).unwrap_or_else(|| panic!("{name} is a menu item"));
    item["states"].as_array().is_some_and(|s| s.iter().any(|s| s == "checked"))
}

fn texts(rt: &Runtime<Ui>, key: &str) -> Vec<String> {
    rt.elements().into_iter().filter(|e| !e.exiting && e.key.as_deref() == Some(key)).filter_map(|e| e.text).collect()
}

#[test]
fn the_menu_lists_volumes_with_kind_and_free_space() {
    let (ui, _) = mac();
    let mut rt = runtime(ui);
    click(&mut rt, "source-0");
    assert_eq!(rt.state.source_menu.map(|m| m.pane), Some(0));
    for (name, detail) in [
        ("Macintosh HD", "Internal disk, 212 GB free"),
        ("CMG-512M2", "External disk, 340 GB free"),
        ("Studio Share", "Network share, 2.6 TB free"),
        ("Installer", "Disk image, 1.2 MB free"),
    ] {
        assert!(labelled(&rt, name, "menuitemcheckbox").is_some(), "{name} is listed");
        assert!(texts(&rt, "description").iter().any(|t| t == detail), "{name} says “{detail}”");
        // A screen reader hears "CMG-512M2, External disk, 340 GB free".
        let node = a11y_item(&rt, name).unwrap_or_else(|| panic!("{name} in {}", rt.accessibility_json()));
        assert_eq!(node["description"].as_str(), Some(detail), "{node}");
    }
    assert!(texts(&rt, "label").iter().any(|t| t == "This Mac"));
    assert!(rt.elements().iter().any(|e| e.text.as_deref() == Some("Locations")), "the volumes are grouped");
    // The pane is in the home folder: the boot disk carries the tick, not the SSD.
    assert!(ticked(&rt, "Macintosh HD"));
    assert!(!ticked(&rt, "CMG-512M2"));
    assert!(!ticked(&rt, "This Mac"));
}

#[test]
fn choosing_a_volume_goes_there_and_back_still_works() {
    let (ui, _) = mac();
    let mut rt = runtime(ui);
    click(&mut rt, "source-0");
    choose(&mut rt, "CMG-512M2");
    assert!(rt.state.source_menu.is_none(), "the menu closes");
    let pane = &rt.state.panes[0];
    assert_eq!(pane.dir, RPath::new(SSD));
    assert!(pane.is_local());
    assert!(pane.entry("Photos").is_some() && pane.entry("b.txt").is_some(), "the SSD is listed");
    // The header names the SSD and its kind instead of This Mac.
    assert!(texts(&rt, "source-title").contains(&"CMG-512M2".to_owned()));
    assert!(texts(&rt, "source-detail").contains(&"External disk".to_owned()));
    assert_eq!(rt.state.panes[1].dir, RPath::new(HOME), "the other pane stays");
    // Back returns to the home folder, where the header says This Mac again.
    rt.update(|s, _| s.go_back(0));
    step(&mut rt);
    assert_eq!(rt.state.panes[0].dir, RPath::new(HOME));
    let titles = texts(&rt, "source-title");
    assert_eq!(titles.iter().filter(|t| *t == "This Mac").count(), 2, "{titles:?}");
    assert!(texts(&rt, "source-detail").iter().all(|t| t == "Local disk"));
    // Reopened inside the SSD, the tick follows.
    rt.update(|s, _| s.show_volume(0, &disk(SSD)));
    step(&mut rt);
    click(&mut rt, "source-0");
    assert!(ticked(&rt, "CMG-512M2"));
    assert!(!ticked(&rt, "Macintosh HD"));
}

#[test]
fn a_volume_from_a_server_pane_switches_to_this_mac() {
    let (mut ui, _) = mac();
    let web: Arc<dyn Remote> = Arc::new(FakeRemote::web_server());
    ui.sessions.push(crate::ui::Session { id: 31, site: filewinch_core::Site::new("w", "web", filewinch_core::Protocol::Sftp), remote: web.clone() });
    ui.panes[1].switch_to(Source { session: 31, label: "web".into(), protocol: filewinch_core::Protocol::Sftp, remote: web }, RPath::new("/var/www/site"), false);
    ui.show_volume(1, &disk(SHARE));
    assert!(ui.panes[1].is_local());
    assert_eq!(ui.panes[1].dir, RPath::new(SHARE));
    assert!(ui.panes[1].back.is_empty(), "a server's folders aren't Back history on This Mac");
    assert!(ui.session(31).is_none(), "the unused session closes");
    assert_eq!(ui.shown_volume(1).map(|v| v.kind), Some(VolumeKind::Network));
}

#[test]
fn deepest_volume_names_the_header() {
    let (mut ui, _) = mac();
    let mut list = volumes();
    list.push(vol("Inner", &format!("{SSD}/Photos"), VolumeKind::DiskImage, false, 5));
    ui.volumes.set(list);
    ui.panes[0].go(RPath::new(&format!("{SSD}/Photos")), false);
    assert_eq!(ui.shown_volume(0).map(|v| v.name.as_str()), Some("Inner"));
    ui.panes[0].go(RPath::new(SSD), false);
    assert_eq!(ui.shown_volume(0).map(|v| v.name.as_str()), Some("CMG-512M2"));
    ui.panes[0].go(RPath::new(HOME), false);
    assert_eq!(ui.shown_volume(0), None, "the boot disk keeps This Mac");
}

#[test]
fn an_unmounted_volume_says_so_and_offers_home() {
    let (mut ui, remote) = mac();
    ui.show_volume(0, &disk(SSD));
    let mut rt = runtime(ui);
    step(&mut rt);
    assert!(rt.state.panes[0].volume.is_some(), "the pane remembers its volume");
    // The SSD is pulled: its files vanish and the next volume list no longer has it.
    for f in [format!("{SSD}/b.txt"), format!("{SSD}/Photos/a.jpg")] {
        remote.remove_file(&RPath::new(&f)).unwrap();
    }
    remote.remove_dir(&RPath::new(&format!("{SSD}/Photos"))).unwrap();
    remote.remove_dir(&RPath::new(SSD)).unwrap();
    rt.update(|s, _| {
        s.volumes.set(volumes().into_iter().filter(|v| v.name != "CMG-512M2").collect());
        s.refresh_pane(0);
    });
    step(&mut rt);
    assert!(matches!(rt.state.panes[0].listing, Listing::Failed(_)));
    assert_eq!(rt.state.disconnected(0), Some("CMG-512M2"));
    let shown: Vec<String> = rt.elements().into_iter().filter_map(|e| e.text).collect();
    assert!(shown.iter().any(|t| t == "“CMG-512M2” was disconnected."), "{shown:?}");
    assert!(rt.find("go-home").is_some());
    // The header still names the disk that went.
    assert!(texts(&rt, "source-title").contains(&"CMG-512M2".to_owned()));
    click(&mut rt, "go-home");
    let pane = &rt.state.panes[0];
    assert_eq!(pane.dir, remote.home().unwrap());
    assert!(pane.is_ready());
    assert_eq!(rt.state.disconnected(0), None);
    assert!(pane.volume.is_none() || remote.home().unwrap().as_str().starts_with(SSD), "leaving the volume forgets it");
}

#[test]
fn a_folder_that_fails_on_a_mounted_volume_is_an_ordinary_error() {
    let (mut ui, _) = mac();
    ui.panes[0].go(RPath::new(&format!("{SSD}/missing")), false);
    assert!(matches!(ui.panes[0].listing, Listing::Failed(_)));
    assert_eq!(ui.disconnected(0), None, "the SSD is still in the list");
}

#[test]
fn escape_closes_the_menu() {
    let (ui, _) = mac();
    let mut rt = runtime(ui);
    click(&mut rt, "source-1");
    assert!(rt.state.source_menu.is_some());
    rt.key(rune::KeyEvent { key: rune::Key::Escape, mods: rune::Mods::default() });
    step(&mut rt);
    assert!(rt.state.source_menu.is_none());
}
