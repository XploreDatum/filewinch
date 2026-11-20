//! "Saved Servers" at the top of the connect sheet: the favorites, most recently used first,
//! each a click (or Return) from connecting in the sheet's pane, with Edit… to load it into
//! the form below. Hidden when there are no favorites; a filter shows past six.

use filewinch_core::{Protocol, Site};
use filewinch_store::{Favorite, Recents, SitesFile};
use rune::{El, Key, KeyEvent, PointerPress, PopupItem, PopupMenu, ScrollSpec, Tx, col, icon, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit, TextInput};

use super::form::{Field, Form};
use crate::icons;
use crate::theme::Paint;
use crate::ui::{Sheet, Ui};

/// Rows shown before the list scrolls, and past how many favorites the filter appears.
pub const VISIBLE_ROWS: usize = 5;
pub const FILTER_FROM: usize = 7;
const ROW_H: f32 = 34.0;

/// The favorites matching `query` (as the sidebar's search matches), the most recently used
/// first, then the rest by name as people read it.
pub fn order<'a>(sites: &'a SitesFile, recents: &Recents, query: &str) -> Vec<&'a Favorite> {
    let used = |f: &Favorite| recents.items.iter().find(|r| r.site.id == f.site.id).map(|r| r.at);
    let mut v: Vec<&Favorite> = sites.search(query).collect();
    v.sort_by(|a, b| {
        used(b).cmp(&used(a)).then_with(|| rune_kit::natural_cmp(&a.site.name, &b.site.name)).then_with(|| a.site.id.cmp(&b.site.id))
    });
    v
}

/// Where a saved server goes: `user@host` for servers; the account for cloud drives; the
/// bucket (or endpoint) for object stores, never an access key.
pub fn detail(site: &Site) -> String {
    let or_label = |s: &str| if s.is_empty() { site.protocol.label().to_owned() } else { s.to_owned() };
    match site.protocol {
        p if p.uses_oauth() => or_label(&site.user),
        Protocol::S3 | Protocol::B2 | Protocol::Swift => {
            let bucket = site.initial_path.as_deref().and_then(|p| p.trim_matches('/').split('/').next()).filter(|b| !b.is_empty());
            or_label(bucket.unwrap_or(&site.host))
        }
        Protocol::AzureBlob => or_label(&site.host),
        _ => match (site.user.is_empty(), site.host.is_empty()) {
            (false, false) => format!("{}@{}", site.user, site.host),
            (true, false) => site.host.clone(),
            (false, true) => site.user.clone(),
            (true, true) => site.protocol.label().to_owned(),
        },
    }
}

fn form_mut(s: &mut Ui) -> Option<&mut Form> {
    match &mut s.sheet {
        Some(Sheet::Connect(f)) => Some(f),
        _ => None,
    }
}

impl Ui {
    /// Connects saved server `id` in the sheet's pane, as the sidebar does: the sheet closes
    /// and the connection's own dialog (progress, Cancel, errors, password, trust) takes over.
    pub fn connect_saved_server(&mut self, id: &str) {
        let fallback = self.connect_pane(self.active);
        let pane = match &self.sheet {
            Some(Sheet::Connect(f)) => f.pane.unwrap_or(fallback),
            _ => fallback,
        };
        if self.sites.get(id).is_none() {
            return;
        }
        crate::oauth::cancel_sign_in();
        self.sheet = None;
        self.connect_favorite(id, pane);
    }

    /// Loads saved server `id` into the form for editing; Save then updates it in place.
    pub fn edit_saved_server(&mut self, id: &str, tx: &mut Tx) {
        let Some(fav) = self.sites.get(id) else { return };
        let mut next = Form::edit(fav);
        next.save = true;
        let Some(f) = form_mut(self) else { return };
        next.pane = f.pane;
        next.suggestions = std::mem::take(&mut f.suggestions);
        next.saved_filter = f.saved_filter.clone();
        next.saved_cursor = f.saved_cursor;
        crate::oauth::cancel_sign_in();
        *f = next;
        tx.focus(format!("field-{:?}", Field::Name), true);
    }

    fn saved_menu(&mut self, id: &str, press: PointerPress, tx: &mut Tx) {
        let menu = PopupMenu::new().item(PopupItem::new("connect", "Connect")).item(PopupItem::new("edit", "Edit…"));
        let id = id.to_owned();
        tx.popup_menu(menu, press, move |s: &mut Ui, picked, tx: &mut Tx| match picked {
            Some("connect") => s.connect_saved_server(&id),
            Some("edit") => s.edit_saved_server(&id, tx),
            _ => {}
        });
    }

    /// Up and Down move between rows (focus and the Tab stop follow); Return connects.
    fn saved_key(&mut self, i: usize, n: usize, id: &str, ev: &KeyEvent, tx: &mut Tx) -> bool {
        let to = match ev.key {
            Key::Enter => {
                self.connect_saved_server(id);
                return true;
            }
            Key::Down => (i + 1).min(n.saturating_sub(1)),
            Key::Up => i.saturating_sub(1),
            Key::Home => 0,
            Key::End => n.saturating_sub(1),
            _ => return false,
        };
        if let Some(f) = form_mut(self) {
            f.saved_cursor = to;
        }
        tx.focus(format!("saved-{to}"), true);
        true
    }
}

fn saved_row(s: &Ui, f: &Form, fav: &Favorite, i: usize, n: usize, k: &Kit, paint: &Paint) -> El {
    let site = &fav.site;
    let current = i == f.saved_cursor.min(n.saturating_sub(1));
    let editing = f.editing.as_deref() == Some(site.id.as_str());
    let where_ = detail(site);
    let group = fav.folder.as_deref().and_then(|g| s.sites.folders.iter().find(|x| x.id == g)).map(|x| x.name.clone());
    let (a, b, c, d) = (site.id.clone(), site.id.clone(), site.id.clone(), site.id.clone());
    let mut main = row()
        .key(format!("saved-{i}"))
        .role("button")
        .label(format!("{}, {}, {where_}, connect", site.name, site.protocol.label()))
        .focusable(true)
        .tab_index(if current { 0 } else { -1 })
        .grow(1.0)
        .min_w(0.0)
        .h(ROW_H)
        .px(8.0)
        .gap(8.0)
        .center_y()
        .on_press(move |s: &mut Ui| s.connect_saved_server(&a))
        .on_key(move |s: &mut Ui, ev: &KeyEvent, tx: &mut Tx| s.saved_key(i, n, &b, ev, tx))
        .on_secondary_press(move |s: &mut Ui, press, tx: &mut Tx| s.saved_menu(&c, press, tx))
        .child(icon(icons::protocol(site.protocol), 16.0).color(k.c.icon_muted).a11y_hidden())
        .child(text(site.name.clone()).font(k.ty.text_small_medium).color(k.c.text_base).truncate().shrink().min_w(40.0).a11y_hidden())
        .child(text(where_).font(k.ty.text_xsmall).color(k.c.text_muted).truncate().shrink().grow(1.0).min_w(0.0).a11y_hidden());
    if let Some(g) = group {
        main = main.child(
            row().h(18.0).px(6.0).center_y().radius(9.0).fill(k.c.hover).border(1.0, k.c.border_base).a11y_hidden()
                .child(text(g).font(k.ty.text_xsmall).color(k.c.text_muted).truncate()),
        );
    }
    let edit = Button::new("Edit…").variant(ButtonVariant::Ghost).control_size(ControlSize::Small)
        .on_press_tx(move |s: &mut Ui, tx: &mut Tx| s.edit_saved_server(&d, tx))
        .build(k)
        .key(format!("saved-edit-{i}"))
        .label(format!("Edit {}", site.name))
        .tab_index(if current { 0 } else { -1 })
        // Shown while the row is hovered or has keyboard focus.
        .group_opacity(0.0, 1.0);
    let r = row().key(format!("saved-row-{i}")).w_full().pr(4.0).gap(4.0).center_y().radius(6.0).hover_group().hover(k.c.hover).child(main).child(edit);
    if editing { r.fill(k.alpha(paint.accent, 0.12)) } else { r }
}

/// The section, or `None` with no favorites.
pub fn section(s: &Ui, f: &Form, k: &Kit, paint: &Paint) -> Option<El> {
    if s.sites.favorites.is_empty() {
        return None;
    }
    let favs = order(&s.sites, &s.recents, &f.saved_filter.text);
    let n = favs.len();
    let mut right = col().gap(6.0).grow(1.0).min_w(0.0);
    if s.sites.favorites.len() >= FILTER_FROM {
        let t = TextInput::new(&f.saved_filter);
        let t = if f.saved_filter.text.is_empty() { t } else { t.clear_button() };
        right = right.child(
            t.label("Filter saved servers")
                .placeholder("Filter")
                .leading_icon(&icons::SEARCH)
                .height(26.0)
                .width(None)
                .on_edit(|s: &mut Ui| match &mut s.sheet {
                    Some(Sheet::Connect(f)) => &mut f.saved_filter,
                    _ => &mut s.sidebar_search,
                })
                .on_change(|s: &mut Ui, _tx: &mut Tx| {
                    if let Some(f) = form_mut(s) {
                        f.saved_cursor = 0;
                    }
                })
                .on_submit(|s: &mut Ui, _tx: &mut Tx| {
                    let first = form_mut(s).map(|f| f.saved_filter.text.clone()).and_then(|q| order(&s.sites, &s.recents, &q).first().map(|f| f.site.id.clone()));
                    if let Some(id) = first {
                        s.connect_saved_server(&id);
                    }
                })
                .on_key(|s: &mut Ui, ev: &KeyEvent, tx: &mut Tx| {
                    if ev.key != Key::Down {
                        return false;
                    }
                    if let Some(f) = form_mut(s) {
                        f.saved_cursor = 0;
                    }
                    tx.focus("saved-0", true);
                    true
                })
                .build(k)
                .key("saved-filter"),
        );
    }
    let mut rows = col().key("saved-rows").role("list").label("Saved servers").w_full().padding(3.0);
    for (i, fav) in favs.iter().enumerate() {
        rows = rows.child(saved_row(s, f, fav, i, n, k, paint));
    }
    if favs.is_empty() {
        rows = rows.child(
            text(format!("No saved servers match “{}”.", f.saved_filter.text.trim())).key("saved-none").font(k.ty.text_small).color(k.c.text_muted).padding(8.0),
        );
    }
    let list = col().key("saved-list").w_full().max_h(VISIBLE_ROWS as f32 * ROW_H + 6.0 + 0.5 * ROW_H).min_h(0.0).radius(6.0)
        .fill(paint.list).border(1.0, paint.hair).clip_soft(0.0).scroll(ScrollSpec { stick_bottom: false }).child(rows);
    right = right.child(list);
    Some(
        row().key("saved").w_full().gap(12.0).items_start()
            .child(text("Saved Servers").role("heading").level(3).font(k.ty.text_small).color(k.c.text_muted).w(124.0).align(rune::Align::End).pt(if s.sites.favorites.len() >= FILTER_FROM { 5.0 } else { 9.0 }))
            .child(right),
    )
}
