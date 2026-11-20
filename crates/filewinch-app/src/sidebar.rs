//! The Favorites sidebar: search, groups, saved sites (double-click connects to the active
//! pane), recent connections, and their menus.

use filewinch_core::Site;
use filewinch_store::{Favorite, secret_account};
use rune::{El, PointerPress, PopupItem, PopupMenu, ScrollSpec, TextEdit, Tx, col, icon, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, Kit, TextInput};

use crate::connect::flow::Target;
use crate::connect::form::Form;
use crate::icons;
use crate::theme::Paint;
use crate::ui::{Sheet, Tone, Ui};

fn detail(site: &Site) -> String {
    match (site.user.is_empty(), site.host.is_empty()) {
        (false, false) => format!("{}@{}", site.user, site.host),
        (true, false) => site.host.clone(),
        (false, true) => site.user.clone(),
        (true, true) => site.protocol.label().to_owned(),
    }
}

impl Ui {
    fn favorite_menu(&mut self, id: &str, press: PointerPress, tx: &mut Tx) {
        self.sidebar_selected = Some(id.to_owned());
        let mut groups = PopupMenu::new().item(PopupItem::new("group:", "No Group"));
        for f in &self.sites.folders {
            groups = groups.item(PopupItem::new(format!("group:{}", f.id), f.name.clone()));
        }
        let menu = PopupMenu::new()
            .item(PopupItem::new("connect", format!("Connect in {} Pane", if self.active == 0 { "Left" } else { "Right" })))
            .item(PopupItem::new("connect-other", format!("Connect in {} Pane", if self.active == 0 { "Right" } else { "Left" })))
            .separator()
            .item(PopupItem::new("edit", "Edit…"))
            .item(PopupItem::new("duplicate", "Duplicate"))
            .submenu("Move to Group", groups)
            .separator()
            .item(PopupItem::new("delete", "Delete…"));
        let id = id.to_owned();
        tx.popup_menu(menu, press, move |s: &mut Ui, picked, _tx: &mut Tx| {
            let Some(pick) = picked else { return };
            match pick {
                "connect" => s.connect_favorite(&id, s.active),
                "connect-other" => s.connect_favorite(&id, s.other(s.active)),
                "edit" => s.edit_favorite(&id),
                "duplicate" => s.duplicate_favorite(&id),
                "delete" => {
                    if let Some(f) = s.sites.get(&id) {
                        s.sheet = Some(Sheet::ConfirmForget { site_id: id.clone(), name: f.site.name.clone() });
                    }
                }
                g if g.starts_with("group:") => {
                    let group = g.trim_start_matches("group:");
                    s.sites.move_to(&id, Some(group).filter(|g| !g.is_empty()));
                    s.save_sites();
                }
                _ => {}
            }
        });
    }

    fn group_menu(&mut self, id: &str, press: PointerPress, tx: &mut Tx) {
        let menu = PopupMenu::new().item(PopupItem::new("rename", "Rename Group…")).item(PopupItem::new("delete", "Delete Group"));
        let id = id.to_owned();
        tx.popup_menu(menu, press, move |s: &mut Ui, picked, _tx: &mut Tx| match picked {
            Some("rename") => {
                let name = s.sites.folders.iter().find(|f| f.id == id).map(|f| f.name.clone()).unwrap_or_default();
                s.sheet = Some(Sheet::Group { id: Some(id.clone()), edit: TextEdit::new(name), error: None });
            }
            Some("delete") => {
                s.sites.remove_folder(&id);
                s.save_sites();
            }
            _ => {}
        });
    }

    pub fn edit_favorite(&mut self, id: &str) {
        if let Some(f) = self.sites.get(id) {
            let mut form = Form::edit(f);
            form.save = true;
            self.sheet = Some(Sheet::Connect(Box::new(form)));
        }
    }

    pub fn duplicate_favorite(&mut self, id: &str) {
        let Some((copy, moves)) = self.sites.duplicate(id) else { return };
        let secrets = crate::hub::hub().secrets.clone();
        for (from, to) in moves {
            match secrets.get(&from.0) {
                Ok(Some(v)) => {
                    if let Err(e) = secrets.set(&to.0, &v) {
                        self.notify_error(&e);
                    }
                }
                Ok(None) => {}
                Err(e) => self.notify_error(&e),
            }
        }
        self.sidebar_selected = Some(copy.site.id);
        self.save_sites();
    }

    pub fn forget_favorite(&mut self, id: &str) {
        let Some(f) = self.sites.remove(id) else { return };
        for what in ["password", "passphrase", "oauth"] {
            if let Err(e) = crate::hub::hub().secrets.delete(&secret_account(&f.site.id, what)) {
                self.notify(Tone::Warning, format!("“{}” was removed, but its password is still in {}.", f.site.name, crate::theme::ui_os().secret_store()), Some(e.message));
                break;
            }
        }
        if self.sidebar_selected.as_deref() == Some(id) {
            self.sidebar_selected = None;
        }
        self.save_sites();
    }

    pub fn save_group(&mut self) {
        let Some(Sheet::Group { id, edit, error }) = &mut self.sheet else { return };
        let name = edit.text.trim().to_owned();
        if name.is_empty() {
            *error = Some("A group needs a name.".into());
            return;
        }
        match id.clone() {
            Some(id) => {
                self.sites.rename_folder(&id, &name);
            }
            None => {
                self.sites.add_folder(&name);
            }
        }
        self.sheet = None;
        self.save_sites();
    }

    fn connect_recent(&mut self, i: usize) {
        let Some(r) = self.recents.items.get(i) else { return };
        let site = r.site.clone();
        let saved = self.sites.get(&site.id).cloned();
        let target = match saved {
            Some(f) => Target { site: f.site, pane: self.connect_pane(self.active), secrets: Vec::new(), save: false, folder: f.folder, trusted_once: Vec::new() },
            None => Target { site, pane: self.connect_pane(self.active), secrets: Vec::new(), save: false, folder: None, trusted_once: Vec::new() },
        };
        self.connect(target);
    }
}

fn site_row(s: &Ui, fav: &Favorite, depth: f32, k: &Kit, paint: &Paint) -> El {
    let id = fav.site.id.clone();
    let selected = s.sidebar_selected.as_deref() == Some(id.as_str());
    let connected = s.sessions.iter().any(|x| x.site.id == id);
    let (a, b, c) = (id.clone(), id.clone(), id.clone());
    let mut r = row()
        .key(format!("fav-{id}"))
        .role("treeitem")
        .label(format!("{}, {}{}", fav.site.name, fav.site.protocol.label(), if connected { ", connected" } else { "" }))
        .focus_selected(selected)
        .focusable(true)
        .w_full()
        .h(40.0)
        .pl(10.0 + depth)
        .pr(8.0)
        .gap(9.0)
        .center_y()
        .radius(6.0)
        .fill(if selected { paint.sidebar_selected } else { rune::Color::TRANSPARENT })
        .hover(k.c.hover)
        .on_press(move |s: &mut Ui| s.sidebar_selected = Some(a.clone()))
        .on_double_press(move |s: &mut Ui| s.connect_favorite(&b, s.connect_pane(s.active)))
        .on_secondary_press(move |s: &mut Ui, press, tx: &mut Tx| s.favorite_menu(&c, press, tx))
        .child(icon(icons::protocol(fav.site.protocol), 16.0).color(if connected { paint.accent } else { k.c.icon_muted }).a11y_hidden())
        .child(col().gap(1.0).grow(1.0).min_w(0.0).a11y_hidden()
            .child(text(fav.site.name.clone()).font(k.ty.text_small_medium).color(k.c.text_base).truncate())
            .child(text(detail(&fav.site)).font(k.ty.text_xsmall).color(k.c.text_muted).truncate()));
    if connected {
        r = r.child(rune::el().size(6.0, 6.0).radius(3.0).fill(paint.success).a11y_hidden());
    }
    if selected && let Some(pill) = crate::theme::pill(paint, 40.0) {
        r = r.child(pill);
    }
    r.on_key(move |s: &mut Ui, ev: &rune::KeyEvent, _tx: &mut Tx| {
        if ev.key == rune::Key::Enter {
            s.connect_favorite(&id, s.connect_pane(s.active));
            return true;
        }
        false
    })
}

fn heading(k: &Kit, key: &str, title: &str) -> El {
    text(title.to_owned()).key(key.to_owned()).font(crate::theme::section(k)).color(k.c.text_muted).role("heading").level(2).px(10.0).pt(12.0).pb(4.0)
}

pub fn view(s: &Ui, k: &Kit, paint: &Paint) -> El {
    let search = TextInput::new(&s.sidebar_search);
    let search = if s.sidebar_search.text.is_empty() { search } else { search.clear_button() };
    let search = search
        .width(None)
        .label("Search favorites")
        .placeholder("Search")
        .leading_icon(&icons::SEARCH)
        .on_edit(|s: &mut Ui| &mut s.sidebar_search)
        .build(k)
        .key("fav-search");
    let add = IconButton::new(&icons::PLUS).control_size(ControlSize::Small).label("Add favorite or group").help("New favorite or group")
        .on_press_tx(|_: &mut Ui, tx: &mut Tx| {
            let menu = PopupMenu::new().item(PopupItem::new("site", "New Favorite…")).item(PopupItem::new("group", "New Group…"));
            tx.popup_menu(menu, "fav-add", |s: &mut Ui, picked, _tx: &mut Tx| match picked {
                Some("site") => s.open_connect(),
                Some("group") => s.sheet = Some(Sheet::Group { id: None, edit: TextEdit::new("New Group"), error: None }),
                _ => {}
            });
        })
        .build(k)
        .key("fav-add");
    let q = s.sidebar_search.text.trim().to_owned();
    let matches: Vec<&Favorite> = s.sites.search(&q).collect();
    let mut list = col().key("favorites").role("tree").label("Favorites").focus_group().w_full().gap(1.0);
    if s.sites.favorites.is_empty() {
        let new = Button::new("New Connection…").variant(ButtonVariant::Neutral).control_size(ControlSize::Small)
            .on_press(|s: &mut Ui| s.open_connect()).build(k).key("empty-new");
        list = list.child(col().key("fav-empty").w_full().gap(8.0).padding(14.0).items_start()
            .child(text("No favorites yet").font(k.ty.text_small_medium).color(k.c.text_base))
            .child(text("Save a site when you connect, or import one from your SSH config.").font(k.ty.text_xsmall).color(k.c.text_muted).wrap())
            .child(new));
    } else if matches.is_empty() {
        list = list.child(text(format!("No favorites match “{q}”.")).key("fav-none").font(k.ty.text_small).color(k.c.text_muted).padding(12.0).wrap());
    }
    for folder in &s.sites.folders {
        let inside: Vec<&&Favorite> = matches.iter().filter(|f| f.folder.as_deref() == Some(folder.id.as_str())).collect();
        if inside.is_empty() && !q.is_empty() {
            continue;
        }
        let open = !folder.collapsed || !q.is_empty();
        let (fid, gid) = (folder.id.clone(), folder.id.clone());
        list = list.child(row().key(format!("group-{}", folder.id)).role("treeitem").label(folder.name.clone()).expanded(open).focusable(true)
            .w_full().h(28.0).px(6.0).gap(4.0).center_y().radius(6.0).hover(k.c.hover)
            .on_press(move |s: &mut Ui| {
                if let Some(f) = s.sites.folders.iter_mut().find(|f| f.id == fid) {
                    f.collapsed = !f.collapsed;
                }
                s.save_sites();
            })
            .on_secondary_press(move |s: &mut Ui, press, tx: &mut Tx| s.group_menu(&gid, press, tx))
            .child(icon(if open { k.glyph.chevron_down } else { &icons::FORWARD }, 14.0).color(k.c.icon_muted).a11y_hidden())
            .child(icon(&icons::FOLDER, 14.0).color(k.c.icon_muted).a11y_hidden())
            .child(text(folder.name.clone()).font(k.ty.text_small_medium).color(k.c.text_base).truncate().grow(1.0).a11y_hidden())
            .child(text(inside.len().to_string()).font(k.ty.text_xsmall).color(k.c.text_faint).a11y_hidden()));
        if open {
            for f in inside {
                list = list.child(site_row(s, f, 14.0, k, paint));
            }
        }
    }
    let known: std::collections::HashSet<&str> = s.sites.folders.iter().map(|f| f.id.as_str()).collect();
    for f in matches.iter().filter(|f| f.folder.as_deref().is_none_or(|g| !known.contains(g))) {
        list = list.child(site_row(s, f, 0.0, k, paint));
    }
    let mut body = col().w_full().gap(2.0).child(heading(k, "fav-heading", "Favorites")).child(list);
    if !s.recents.items.is_empty() && q.is_empty() {
        let mut recent = col().key("recents").role("list").label("Recent connections").w_full().gap(1.0);
        for (i, r) in s.recents.items.iter().take(6).enumerate() {
            recent = recent.child(row().key(format!("recent-{i}")).role("listitem").label(format!("{}, recent", r.site.name)).focusable(true)
                .w_full().h(28.0).px(10.0).gap(9.0).center_y().radius(6.0).hover(k.c.hover)
                .on_double_press(move |s: &mut Ui| s.connect_recent(i))
                .on_key(move |s: &mut Ui, ev: &rune::KeyEvent, _tx: &mut Tx| {
                    if ev.key == rune::Key::Enter {
                        s.connect_recent(i);
                        return true;
                    }
                    false
                })
                .child(icon(icons::protocol(r.site.protocol), 14.0).color(k.c.icon_faint).a11y_hidden())
                .child(text(r.site.name.clone()).font(k.ty.text_small).color(k.c.text_muted).truncate().grow(1.0).a11y_hidden()));
        }
        body = body.child(heading(k, "recent-heading", "Recent")).child(recent);
    }
    col().key("sidebar").role("navigation").label("Sidebar").w(236.0).h_full().min_h(0.0).fill(paint.chrome).border_sides([0.0, 1.0, 0.0, 0.0], paint.hair)
        .child(row().w_full().gap(6.0).px(10.0).pt(8.0).pb(6.0).center_y().child(search.grow(1.0)).child(add))
        .child(col().w_full().grow(1.0).min_h(0.0).px(6.0).pb(10.0).scroll(ScrollSpec { stick_bottom: false }).child(body))
}
