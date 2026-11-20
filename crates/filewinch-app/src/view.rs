//! The main window: toolbar, Favorites sidebar, two panes, the Transfers drawer, notices,
//! and whichever sheet is open.

use rune::{Cx, El, Tx, col, icon, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, Dialog, IconButton, IconButtonVariant, Kit, TextInput};

use crate::icons;
use crate::theme::{self, Paint};
use crate::ui::{Sheet, Tone, Ui};

fn tool(k: &Kit, key: &str, glyph: &'static rune::Icon, label: &str, on: bool, f: impl Fn(&mut Ui) + 'static) -> El {
    tool_enabled(k, key, glyph, label, on, true, f)
}

fn tool_enabled(k: &Kit, key: &str, glyph: &'static rune::Icon, label: &str, on: bool, enabled: bool, f: impl Fn(&mut Ui) + 'static) -> El {
    let b = IconButton::new(glyph).variant(IconButtonVariant::Ghost).control_size(ControlSize::Regular).label(label).help(label).disabled(!enabled)
        .on_press(f).build(k).key(key.to_owned());
    if on { b.fill(k.c.hover).radius(6.0) } else { b }
}

fn toolbar(s: &Ui, k: &Kit, paint: &Paint, inset: f32) -> El {
    let connect = Button::new("Connect").icon(&icons::PLUS).variant(ButtonVariant::Neutral).control_size(ControlSize::Small).shortcut(crate::keys::Act::Connect.label())
        .on_press(|s: &mut Ui| s.open_connect()).build(k).key("tb-connect");
    let summary = s.transfers.summary();
    let mut centre = row().gap(8.0).center_y().child(icon(&icons::WINCH, 16.0).color(paint.accent).a11y_hidden())
        .child(text("Filewinch").font(k.ty.text_small_medium).color(k.c.text_base));
    if let Some(sum) = summary {
        centre = centre.child(text(format!("· {sum}")).key("tb-summary").font(k.ty.text_xsmall).color(k.c.text_muted));
    }
    let mut bar = row().key("toolbar").role("toolbar").label("Toolbar").w_full().h(46.0).pl(inset.max(12.0) + 6.0).pr(10.0).gap(4.0).center_y().fill(paint.chrome)
        .border_sides([0.0, 0.0, 1.0, 0.0], paint.hair).window_drag();
    // The title: at the leading edge on Windows, centred on macOS and Linux.
    let (lead, centre) = if paint.title_leading { (Some(centre.mr(12.0)), None) } else { (None, Some(centre)) };
    if let Some(title) = lead {
        bar = bar.child(title);
    }
    bar = bar
        .child(tool(k, "tb-sidebar", &icons::STAR, "Show or hide Favorites", s.prefs.sidebar_open, |s: &mut Ui| s.prefs.sidebar_open = !s.prefs.sidebar_open))
        .child(connect.ml(4.0))
        .child(rune::el().grow(1.0));
    if let Some(centre) = centre {
        bar = bar.child(centre).child(rune::el().grow(1.0));
    }
    let bar = bar
        // Sync needs a folder in both panes.
        .child(tool_enabled(k, "tb-sync", &icons::SYNC, "Synchronize folders", false, s.empty_pane().is_none(), |s: &mut Ui| s.open_sync()))
        .child(tool(k, "tb-transfers", &icons::TRANSFERS, "Show or hide Transfers", s.prefs.transfers_open, |s: &mut Ui| s.prefs.transfers_open = !s.prefs.transfers_open))
        .child(tool(k, "tb-inspector", &crate::inspector::ICON, "Show or hide the Inspector", s.prefs.inspector_open, |s: &mut Ui| s.toggle_inspector()))
        .child(tool(k, "tb-settings", &icons::GEAR, crate::keys::Act::Settings.title().trim_end_matches('…'), false, |s: &mut Ui| crate::settings::open_settings(&s.prefs)));
    if crate::theme::ui_os() == filewinch_os::Os::Mac {
        return bar;
    }
    // Windows and Linux have no menu bar: everything it holds is in this menu.
    bar.child(
        IconButton::new(&icons::MENU).variant(IconButtonVariant::Ghost).control_size(ControlSize::Regular).label("Menu").help("Menu")
            .on_press_tx(|s: &mut Ui, tx: &mut Tx| s.app_menu(tx)).build(k).key("tb-menu"),
    )
}

fn notices(s: &Ui, k: &Kit, paint: &Paint) -> Option<El> {
    if s.notices.is_empty() {
        return None;
    }
    let banner = if s.license.is_registered() { 0.0 } else { crate::license_ui::banner::HEIGHT };
    let mut c = col().key("notices").gap(8.0).w(380.0).pin(None, None, Some(16.0), Some(banner + if s.prefs.transfers_open { 316.0 } else { 16.0 }));
    for n in &s.notices {
        let id = n.id;
        let tone = match n.tone {
            Tone::Info => paint.accent,
            Tone::Success => paint.success,
            Tone::Warning => paint.warning,
            Tone::Error => paint.danger,
        };
        let mut body = col().gap(4.0).grow(1.0).min_w(0.0).child(text(n.text.clone()).font(k.ty.text_small).color(k.c.text_base).wrap());
        if let Some(d) = &n.detail {
            let open = n.shown_detail;
            body = body.child(row().key("notice-details").role("button").label(if open { "Hide details" } else { "Show details" }).h(18.0).hover(k.c.hover).radius(3.0)
                .on_press(move |s: &mut Ui| if let Some(n) = s.notices.iter_mut().find(|n| n.id == id) { n.shown_detail = !n.shown_detail })
                .child(text(if open { "Hide details" } else { "Details" }).font(k.ty.text_xsmall_medium).color(k.c.text_muted)));
            if open {
                body = body.child(text(d.clone()).font(k.ty.mono).color(k.c.text_muted).wrap().selectable(true));
            }
        }
        let role = if matches!(n.tone, Tone::Error | Tone::Warning) { "alert" } else { "status" };
        c = c.child(k.elevate(row().key(format!("notice-{id}")).role(role).w_full().gap(10.0).padding(12.0).items_start().radius(8.0).fill(k.c.surface_float)
            .border(1.0, k.alpha(tone, 0.45))
            .child(icon(if matches!(n.tone, Tone::Error | Tone::Warning) { &icons::WARNING } else { &icons::INFO }, 16.0).color(tone).a11y_hidden())
            .child(body)
            .child(IconButton::new(k.glyph.close).variant(IconButtonVariant::Ghost).control_size(ControlSize::Small).label("Dismiss")
                .on_press(move |s: &mut Ui| s.dismiss_notice(id)).build(k).key("dismiss")), &k.elevation.floating));
    }
    Some(c)
}

fn confirm(k: &Kit, layer: &mut Vec<El>, key: &str, title: &str, message: String, action: &str, f: fn(&mut Ui)) {
    let body = col().key(format!("{key}-body")).gap(14.0).px(16.0).pb(16.0).w_full()
        .child(text(message).font(k.ty.text_small).color(k.c.text_base).wrap())
        .child(row().w_full().gap(8.0).child(rune::el().grow(1.0))
            .child(Button::new("Cancel").variant(ButtonVariant::Ghost).on_press(|s: &mut Ui| s.sheet = None).build(k).key("confirm-cancel"))
            .child(Button::new(action).variant(ButtonVariant::Danger).on_press(f).build(k).key("confirm-ok").default_button()));
    Dialog::new(key, title).open(true).width(440.0).fit().child(body).on_close(|s: &mut Ui| s.sheet = None).build(k, layer);
}

fn sheets(s: &Ui, k: &Kit, paint: &Paint, layer: &mut Vec<El>) {
    match &s.sheet {
        None => {}
        Some(Sheet::Connect(f)) => crate::connect::view::sheet(s, f, k, paint, layer),
        Some(Sheet::Info(i)) => crate::info::view(i, k, paint, layer),
        Some(Sheet::Sync(sync)) => crate::sync_sheet::view(sync, k, paint, layer),
        Some(Sheet::ConfirmDelete { names, pane, .. }) => {
            let site = s.panes[*pane].label();
            let what = match names.as_slice() {
                [one] => format!("“{one}”"),
                many => format!("these {} items", many.len()),
            };
            confirm(k, layer, "delete", "Delete?", format!("Delete {what} from {site}? This can't be undone."), "Delete", |s: &mut Ui| {
                if let Some(Sheet::ConfirmDelete { pane, paths, .. }) = s.sheet.take() {
                    s.delete(pane, paths);
                }
            });
        }
        Some(Sheet::DeleteShared(d)) => crate::delete_sheet::view(d, k, paint, layer),
        Some(Sheet::License(l)) => crate::license_ui::sheet::view(l, k, paint, layer),
        Some(Sheet::ConfirmForget { name, .. }) => {
            confirm(k, layer, "forget", "Delete Favorite?", format!("Remove “{name}” from Favorites and its password from {}?", crate::theme::ui_os().secret_store()), "Delete", |s: &mut Ui| {
                if let Some(Sheet::ConfirmForget { site_id, .. }) = s.sheet.take() {
                    s.forget_favorite(&site_id);
                }
            });
        }
        Some(Sheet::Group { id, edit, error }) => {
            let field = TextInput::new(edit).label("Group name").placeholder("Clients").invalid(error.is_some())
                .on_edit(|s: &mut Ui| match &mut s.sheet {
                    Some(Sheet::Group { edit, .. }) => edit,
                    _ => &mut s.sidebar_search,
                })
                .on_submit(|s: &mut Ui, _tx: &mut Tx| s.save_group()).build(k).key("group-name");
            let mut body = col().key("group-body").gap(12.0).px(16.0).pb(16.0).w_full().child(field);
            if let Some(e) = error {
                body = body.child(text(e.clone()).font(k.ty.text_xsmall).color(paint.danger).live_region(rune::LiveRegion::Polite));
            }
            body = body.child(row().w_full().gap(8.0).child(rune::el().grow(1.0))
                .child(Button::new("Cancel").variant(ButtonVariant::Ghost).on_press(|s: &mut Ui| s.sheet = None).build(k).key("group-cancel"))
                .child(Button::new(if id.is_some() { "Rename" } else { "Create" }).variant(ButtonVariant::Contrast).on_press(|s: &mut Ui| s.save_group()).build(&theme::primary(k)).key("group-ok").default_button()));
            Dialog::new("group", if id.is_some() { "Rename Group" } else { "New Group" }).open(true).width(380.0).fit().child(body)
                .on_close(|s: &mut Ui| s.sheet = None).build(k, layer);
        }
    }
}

pub fn view(s: &Ui, cx: &mut Cx<Ui>) -> El {
    let dark = theme::is_dark(s.prefs.appearance, cx.system_dark());
    theme::follow(cx, dark);
    let k = &*theme::kit(dark);
    let paint = theme::paint(k);
    let mut layer = Vec::new();
    let panes = row().key("panes").grow(1.0).w_full().min_h(0.0).gap(8.0).padding(8.0)
        .child(crate::pane::bar::pane(s, 0, k, &paint, cx, &mut layer))
        .child(crate::pane::bar::pane(s, 1, k, &paint, cx, &mut layer));
    let mut main = row().key("main").w_full().grow(1.0).min_h(0.0);
    if s.prefs.sidebar_open {
        main = main.child(crate::sidebar::view(s, k, &paint));
    }
    main = main.child(panes);
    let main = crate::inspector::attach(s, main, k, &paint, &mut layer);
    let mut window = col().key("window").w_full().h_full().fill(paint.chrome)
        .child(toolbar(s, k, &paint, cx.titlebar_inset()))
        .child(main);
    if let Some(banner) = crate::license_ui::banner::view(s, k, &paint) {
        window = window.child(banner);
    }
    if s.prefs.transfers_open {
        window = window.child(crate::transfers::view::drawer(s, k, &paint));
    }
    if let Some(n) = notices(s, k, &paint) {
        layer.push(n);
    }
    sheets(s, k, &paint, &mut layer);
    crate::connect::view::conn_dialog(s, k, &paint, &mut layer);
    rune_kit::root(window, layer)
}
