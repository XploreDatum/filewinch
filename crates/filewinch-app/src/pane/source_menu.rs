//! The menu under a pane's location chip: This Mac, the mounted volumes ("Locations"), the
//! servers already connected, Connect to Favorite, New Connection… and Disconnect.
//!
//! It's drawn (rune-kit's `Menu`), not a native pop-up, so each volume carries its kind's icon
//! and free space, and the same menu works on Linux, where Rune has no native pop-ups.

use rune::El;
use rune_kit::{Kit, Menu, MenuEntry, MenuItem};

use crate::icons;
use crate::ui::Ui;

/// The open location menu: which pane's, and whether Connect to Favorite is expanded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceMenu {
    pub pane: usize,
    pub favorites: bool,
}

/// "External disk, 340 GB free": the second line under a volume's name, which screen readers
/// also read after the name.
pub fn volume_detail(v: &filewinch_os::volumes::Volume) -> String {
    match filewinch_os::volumes::free_text(v) {
        Some(free) => format!("{}, {free}", v.kind.describe()),
        None => v.kind.describe().to_owned(),
    }
}

impl Ui {
    /// Opens pane `p`'s location menu and reads the volumes again; the menu shows the last
    /// list until the fresh one lands.
    pub fn open_source_menu(&mut self, p: usize) {
        self.source_menu = Some(SourceMenu { pane: p, favorites: false });
        self.volumes.refresh();
    }

    fn close_source_menu(&mut self) {
        self.source_menu = None;
    }
}

/// The location chip `trigger`, with its menu in `layer` while it's open.
pub fn menu(s: &Ui, p: usize, trigger: El, k: &Kit, layer: &mut Vec<El>) -> El {
    let open = s.source_menu.filter(|m| m.pane == p);
    let pane = &s.panes[p];
    // The volume the pane is inside (the boot disk too), for its tick.
    let here = pane.is_local().then(|| s.volumes.containing(&pane.dir)).flatten().map(|v| v.path.clone());
    let mut m = Menu::new(format!("srcmenu-{p}")).trigger(trigger).open(open.is_some()).min_width(250.0).on_close(|s: &mut Ui| s.close_source_menu());
    if open.is_none() {
        return m.build(k, layer).min_w(0.0).shrink();
    }
    // This Mac goes to the home folder; it's ticked when no volume below claims the pane.
    m = m.item(
        MenuItem::new(crate::theme::ui_os().this_computer())
            .icon(&icons::LAPTOP)
            .description("Your home folder")
            .checked(pane.is_local() && here.is_none())
            .on_select(move |s: &mut Ui| {
                s.close_source_menu();
                s.show_local(p);
            }),
    );
    if !s.volumes.list.is_empty() {
        m = m.group_label("Locations");
        for v in &s.volumes.list {
            let path = v.path.clone();
            m = m.item(
                MenuItem::new(v.name.clone())
                    .icon(icons::volume(v.kind))
                    .description(volume_detail(v))
                    .checked(here.as_deref() == Some(v.path.as_path()))
                    .on_select(move |s: &mut Ui| {
                        s.close_source_menu();
                        s.show_volume(p, &path);
                    }),
            );
        }
    }
    if !s.sessions.is_empty() {
        m = m.group_label("Connected");
        for sess in &s.sessions {
            let id = sess.id;
            m = m.item(
                MenuItem::new(sess.site.name.clone())
                    .icon(icons::protocol(sess.site.protocol))
                    .checked(pane.session() == Some(id))
                    .on_select(move |s: &mut Ui| {
                        s.close_source_menu();
                        s.show_session(p, id);
                    }),
            );
        }
    }
    let favs: Vec<MenuEntry> = s
        .sites
        .favorites
        .iter()
        .filter(|f| !s.sessions.iter().any(|x| x.site.id == f.site.id))
        .take(12)
        .map(|f| {
            let id = f.site.id.clone();
            MenuEntry::item(MenuItem::new(f.site.name.clone()).icon(icons::protocol(f.site.protocol)).on_select(move |s: &mut Ui| {
                s.close_source_menu();
                s.connect_favorite(&id, p);
            }))
        })
        .collect();
    let any = !favs.is_empty();
    let expanded = open.is_some_and(|o| o.favorites) && any;
    m = m
        .separator()
        .item(
            MenuItem::new("Connect to Favorite").icon(&icons::STAR).submenu(favs, expanded).disabled(!any).on_select(move |s: &mut Ui| {
                if let Some(o) = &mut s.source_menu {
                    o.favorites = !o.favorites;
                }
            }),
        )
        .item(MenuItem::new("New Connection…").icon(&icons::PLUS).on_select(move |s: &mut Ui| {
            s.close_source_menu();
            s.open_connect_in(p);
        }));
    if !pane.is_local() && !pane.is_empty() {
        m = m.separator().item(MenuItem::new("Disconnect").icon(&icons::EJECT).on_select(move |s: &mut Ui| {
            s.close_source_menu();
            s.disconnect(p);
        }));
    }
    m.build(k, layer).min_w(0.0).shrink()
}
