//! A pane's chrome: which site it shows, back/forward/up, the path bar (breadcrumbs, or a
//! field to type a path), the filter, and the status line.

use filewinch_core::RPath;
use rune::{Cx, El, PopupItem, PopupMenu, TextEdit, Tx, col, icon, row, text};
use rune_kit::{ControlSize, IconButton, IconButtonVariant, Kit, Spinner, TextInput};

use super::layout::fit_crumbs;
use super::view::table;
use crate::icons;
use crate::theme::Paint;
use crate::ui::Ui;

impl Ui {
    pub fn edit_path(&mut self, p: usize, tx: &mut Tx) {
        let pane = &mut self.panes[p];
        // This computer's folders read as the OS writes them (`C:\\Users\\me` on Windows).
        let shown = if pane.is_local() && cfg!(windows) && !pane.dir.is_root() {
            filewinch_local::local_path(&pane.dir).display().to_string()
        } else {
            pane.dir.to_string()
        };
        let mut edit = TextEdit::new(shown);
        edit.anchor = 0;
        edit.focus = edit.text.len();
        pane.path_edit = Some(edit);
        tx.focus(format!("path-{p}"), true);
    }

    pub fn commit_path(&mut self, p: usize, tx: &mut Tx) {
        let hidden = self.prefs.show_hidden;
        let pane = &mut self.panes[p];
        let Some(edit) = pane.path_edit.take() else { return };
        let typed = edit.text.trim();
        if pane.is_empty() {
            return;
        }
        // On this computer the typed path is the OS's own: `~/x`, and on Windows `C:\\x` too.
        let os_path = std::path::Path::new(typed);
        let dir = if pane.is_local() && (typed == "~" || typed.starts_with("~/") || typed.starts_with("~\\")) {
            filewinch_local::rpath(&filewinch_os::dirs::expand_tilde(typed))
        } else if pane.is_local() && cfg!(windows) && os_path.is_absolute() {
            filewinch_local::rpath(os_path)
        } else if typed.starts_with('/') {
            RPath::new(typed)
        } else {
            pane.dir.join(typed)
        };
        pane.go(dir, hidden);
        tx.focus(format!("pane-{p}"), true);
    }

    fn path_edit_mut(&mut self, p: usize) -> &mut TextEdit {
        match &mut self.panes[p].path_edit {
            Some(e) => e,
            None => &mut self.panes[p].filter,
        }
    }

    /// The folders the path bar folded into "…", to jump to one.
    fn hidden_crumbs_menu(&mut self, p: usize, hidden: Vec<(String, RPath)>, tx: &mut Tx) {
        let mut menu = PopupMenu::new();
        for (i, (label, _)) in hidden.iter().enumerate() {
            menu = menu.item(PopupItem::new(i.to_string(), label.clone()));
        }
        tx.popup_menu(menu, format!("crumbs-{p}/crumb-more"), move |s: &mut Ui, picked, _tx: &mut Tx| {
            if let Some(to) = picked.and_then(|id| id.parse::<usize>().ok()).and_then(|i| hidden.get(i)) {
                let show_hidden = s.prefs.show_hidden;
                s.active = p;
                s.panes[p].go(to.1.clone(), show_hidden);
            }
        });
    }

    fn source_menu(&mut self, p: usize, tx: &mut Tx) {
        let mut menu = PopupMenu::new().item(PopupItem::new("local", crate::theme::ui_os().this_computer()).checked(self.panes[p].is_local()));
        for s in &self.sessions {
            menu = menu.item(PopupItem::new(format!("s:{}", s.id), s.site.name.clone()).checked(self.panes[p].session() == Some(s.id)));
        }
        let fav = self.sites.favorites.iter().filter(|f| !self.sessions.iter().any(|s| s.site.id == f.site.id)).take(12);
        let mut sites = PopupMenu::new();
        let mut any = false;
        for f in fav {
            any = true;
            sites = sites.item(PopupItem::new(format!("f:{}", f.site.id), f.site.name.clone()));
        }
        menu = menu.separator().submenu_enabled("Connect to Favorite", sites, any).item(PopupItem::new("new", "New Connection…"));
        if !self.panes[p].is_local() && !self.panes[p].is_empty() {
            menu = menu.separator().item(PopupItem::new("disconnect", "Disconnect"));
        }
        tx.popup_menu(menu, format!("source-{p}"), move |s: &mut Ui, picked, _tx: &mut Tx| {
            let Some(id) = picked else { return };
            let hidden = s.prefs.show_hidden;
            if id == "local" && !s.panes[p].is_local() {
                s.show_local(p);
            } else if let Some(n) = id.strip_prefix("s:").and_then(|n| n.parse::<u64>().ok()) {
                if let Some(sess) = s.session(n) {
                    let src = crate::pane::Source { session: n, label: sess.site.name.clone(), protocol: sess.site.protocol, remote: sess.remote.clone() };
                    let home = src.remote.home().unwrap_or_else(|_| RPath::root());
                    s.panes[p].switch_to(src, home, hidden);
                    s.active = p;
                }
            } else if let Some(fid) = id.strip_prefix("f:") {
                s.connect_favorite(fid, p);
            } else if id == "new" {
                s.open_connect_in(p);
            } else if id == "disconnect" {
                s.disconnect(p);
            }
        });
    }
}

fn nav_button(k: &Kit, key: String, glyph: &'static rune::Icon, label: &str, enabled: bool, f: impl Fn(&mut Ui) + 'static) -> El {
    IconButton::new(glyph).variant(IconButtonVariant::Ghost).control_size(ControlSize::Small).label(label).help(label).disabled(!enabled)
        .on_press(f).build(k).key(key)
}

/// A crumb chip's padding either side, the room a separator takes (its glyph and the row's gaps),
/// and the room kept free at the bar's end for a click that edits the path.
const CRUMB_PAD: f32 = 5.0;
const CRUMB_SEP: f32 = 12.0;
const EDIT_ROOM: f32 = 16.0;

fn crumbs(s: &Ui, p: usize, k: &Kit, cx: &mut Cx<Ui>) -> El {
    let pane = &s.panes[p];
    let trail = pane.crumbs();
    let n = trail.len();
    // Full names while they fit; when they don't, the folders nearest the root give way to "…"
    // first, and the folder you're in always shows. Re-runs as the bar's width changes.
    let avail = cx.layout_size(&format!("pathbar-{p}")).map(|[w, _]| w - 2.0 * 2.0 - 2.0 - EDIT_ROOM);
    let font = |last: bool| if last { k.ty.text_small_medium } else { k.ty.text_small };
    let width = |t: &str, last: bool| rune::text::measure(t, &[], font(last), None).0 + 2.0 * CRUMB_PAD;
    let first = match avail {
        Some(avail) => {
            let widths: Vec<f32> = trail.iter().enumerate().map(|(i, (label, _))| width(label, i + 1 == n)).collect();
            fit_crumbs(&widths, CRUMB_SEP, width("…", false), avail)
        }
        None => 0,
    };
    let mut r = row().key(format!("crumbs-{p}")).role("navigation").label("Path").gap(1.0).center_y().min_w(0.0).grow(1.0).clip_soft(0.0);
    if first > 0 {
        let hidden: Vec<(String, RPath)> = trail[..first].to_vec();
        let label = format!("{} more folder{}", hidden.len(), if hidden.len() == 1 { "" } else { "s" });
        r = r.child(row().key("crumb-more").role("button").label(label).help(hidden.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>().join(" › "))
            .h(22.0).px(CRUMB_PAD).center_y().radius(4.0).hover(k.c.hover)
            .on_press_tx(move |s: &mut Ui, tx: &mut Tx| s.hidden_crumbs_menu(p, hidden.clone(), tx))
            .child(text("…").font(k.ty.text_small).color(k.c.text_muted).a11y_hidden()));
    }
    for (i, (label, path)) in trail.into_iter().enumerate().skip(first) {
        let last = i + 1 == n;
        if i > first || first > 0 {
            r = r.child(icon(&icons::FORWARD, 10.0).color(k.c.icon_faint).a11y_hidden());
        }
        let to = path.clone();
        let chip = row().key(format!("crumb-{i}")).role("link").label(label.clone()).h(22.0).px(CRUMB_PAD).center_y().radius(4.0).hover(k.c.hover);
        // Only the folder you're in may shrink (and then truncates): it's the one always shown.
        let chip = if last { chip.min_w(0.0).shrink() } else { chip };
        r = r.child(chip
            .on_press(move |s: &mut Ui| {
                let hidden = s.prefs.show_hidden;
                s.active = p;
                s.panes[p].go(to.clone(), hidden);
            })
            .child(text(label.clone()).font(font(last)).color(if last { k.c.text_base } else { k.c.text_muted }).truncate().help_when_truncated(label).a11y_hidden()));
    }
    // A click on the empty end of the bar edits the path.
    r.child(rune::el().key(format!("crumb-edit-{p}")).grow(1.0).min_w(EDIT_ROOM).h(22.0).role("button").label("Edit path").on_press_tx(move |s: &mut Ui, tx: &mut Tx| s.edit_path(p, tx)))
}

fn path_bar(s: &Ui, p: usize, k: &Kit, paint: &Paint, cx: &mut Cx<Ui>) -> El {
    let pane = &s.panes[p];
    let inner = match &pane.path_edit {
        Some(edit) => TextInput::new(edit)
            .label("Go to folder")
            .width(None)
            .placeholder("/path/to/folder")
            .height(24.0)
            .on_edit(move |s: &mut Ui| s.path_edit_mut(p))
            .on_submit(move |s: &mut Ui, tx: &mut Tx| s.commit_path(p, tx))
            .on_key(move |s: &mut Ui, ev: &rune::KeyEvent, tx: &mut Tx| {
                if ev.key == rune::Key::Escape {
                    s.panes[p].path_edit = None;
                    tx.focus(format!("pane-{p}"), true);
                    return true;
                }
                false
            })
            .build(k)
            .key(format!("path-{p}"))
            .grow(1.0),
        None => crumbs(s, p, k, cx),
    };
    row().key(format!("pathbar-{p}")).h(26.0).grow(1.0).min_w(0.0).px(2.0).center_y().radius(k.radius.md).fill(paint.list).border(1.0, paint.hair).child(inner)
}

fn filter(s: &Ui, p: usize, k: &Kit) -> El {
    let t = TextInput::new(&s.panes[p].filter);
    let t = if s.panes[p].filter.text.is_empty() { t } else { t.clear_button() };
    t
        .label("Filter files")
        .placeholder("Filter")
        .leading_icon(&icons::SEARCH)
        .height(26.0)
        .width(Some(132.0))
        .on_edit(move |s: &mut Ui| &mut s.panes[p].filter)
        .on_change(move |s: &mut Ui, _tx: &mut Tx| {
            let hidden = s.prefs.show_hidden;
            s.panes[p].rearrange(hidden);
        })
        .on_key(move |_: &mut Ui, ev: &rune::KeyEvent, tx: &mut Tx| {
            if ev.key == rune::Key::Down || ev.key == rune::Key::Enter {
                tx.focus(format!("pane-{p}"), true);
                return true;
            }
            false
        })
        .build(k)
        .key(format!("filter-{p}"))
}

fn source_header(s: &Ui, p: usize, k: &Kit, paint: &Paint) -> El {
    let pane = &s.panes[p];
    let active = s.active == p;
    let detail = match pane.session().and_then(|id| s.session(id)) {
        // Cloud drives have no host; show the account instead, or just the protocol.
        Some(sess) => {
            let site = &sess.site;
            let place = if site.host.is_empty() { &site.user } else { &site.host };
            if place.is_empty() {
                site.protocol.label().to_owned()
            } else {
                format!("{} · {}", site.protocol.label(), place)
            }
        }
        None if pane.is_empty() => format!("Choose a server or {}", crate::theme::ui_os().this_computer()),
        None => "Local disk".to_owned(),
    };
    let glyph = match &pane.src {
        Some(src) => icons::protocol(src.protocol),
        None => &icons::SERVER,
    };
    let chip = row().key(format!("source-{p}")).role("button").label(format!("{} pane: {}. Change location", if p == 0 { "Left" } else { "Right" }, pane.label()))
        .expanded(false).h(30.0).px(8.0).gap(8.0).center_y().radius(6.0).hover(k.c.hover).min_w(0.0).shrink()
        .on_press_tx(move |s: &mut Ui, tx: &mut Tx| s.source_menu(p, tx))
        .child(icon(glyph, 16.0).color(if active { paint.accent } else { k.c.icon_muted }).a11y_hidden())
        .child(col().min_w(0.0).a11y_hidden()
            .child(text(pane.label().to_owned()).font(k.ty.text_small_medium).color(k.c.text_strong).truncate())
            .child(text(detail).font(k.ty.text_xsmall).color(k.c.text_muted).truncate()))
        .child(icon(k.glyph.chevron_down, 14.0).color(k.c.icon_faint).a11y_hidden());
    let mut r = row().w_full().h(40.0).px(6.0).gap(4.0).center_y().child(chip).child(rune::el().grow(1.0));
    if pane.is_loading() && !pane.entries.is_empty() {
        r = r.child(Spinner::new().size(14.0).build(k).label("Refreshing"));
    }
    if !pane.is_local() && !pane.is_empty() {
        r = r.child(nav_button(k, format!("disconnect-{p}"), &icons::EJECT, "Disconnect", true, move |s: &mut Ui| s.disconnect(p)));
    }
    r
}

/// One whole pane.
pub fn pane(s: &Ui, p: usize, k: &Kit, paint: &Paint, cx: &mut Cx<Ui>) -> El {
    let pane = &s.panes[p];
    let active = s.active == p;
    let frame = col().key(format!("pane-box-{p}")).role("region").label(format!("{} pane", if p == 0 { "Left" } else { "Right" }))
        .grow(1.0).w(0.0).min_w(300.0).h_full().min_h(0.0)
        .fill(paint.list).radius(k.radius.lg).border(1.0, if active { k.alpha(paint.accent, 0.55) } else { paint.hair }).clip_soft(0.0)
        .child(source_header(s, p, k, paint));
    let status = |label: String| row().key(format!("status-{p}")).w_full().h(22.0).px(12.0).center_y().border_sides([1.0, 0.0, 0.0, 0.0], paint.hair)
        .child(text(label).font(k.ty.text_xsmall).color(k.c.text_muted).truncate().live_region(rune::LiveRegion::Polite));
    if pane.is_empty() {
        // No path bar, history, filter or columns: there's nothing to browse yet.
        return frame.child(super::empty::body(p, k, paint)).child(status(pane.status()));
    }
    let nav = row().w_full().h(36.0).px(6.0).gap(2.0).center_y()
        .child(nav_button(k, format!("back-{p}"), &icons::BACK, "Back", !pane.back.is_empty(), move |s: &mut Ui| s.go_back(p)))
        .child(nav_button(k, format!("forward-{p}"), &icons::FORWARD, "Forward", !pane.forward.is_empty(), move |s: &mut Ui| s.go_forward(p)))
        .child(nav_button(k, format!("up-{p}"), &icons::UP, "Enclosing folder", !pane.dir.is_root(), move |s: &mut Ui| s.go_up(p)))
        .child(path_bar(s, p, k, paint, cx).ml(4.0))
        .child(nav_button(k, format!("refresh-{p}"), &icons::REFRESH, "Refresh", true, move |s: &mut Ui| s.refresh_pane(p)))
        .child(filter(s, p, k).ml(2.0));
    frame.child(nav).child(table(s, p, k, paint, cx)).child(status(pane.status()))
}
