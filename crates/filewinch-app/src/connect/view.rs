//! The connect sheet (protocol picker, per-protocol fields with inline errors, "Save as
//! favorite"), and the dialogs a connection can raise: progress with Cancel, failure with
//! Details and Retry, a password re-prompt, and the trust question.

use filewinch_core::{Protocol, TlsMode};
use filewinch_store::Favorite;
use rune::{El, FileDialogOptions, PopupMenu, Tx, col, icon, row, text};
use rune_kit::{Button, ButtonVariant, Checkbox, ControlSize, Dialog, Kit, SegmentedControl, Spinner, TextInput};

use super::flow::{Conn, Target};
use super::form::{AuthChoice, Field, Form};
use crate::hub::hub;
use crate::icons;
use crate::registry;
use crate::theme::Paint;
use crate::ui::{Sheet, Ui};

fn form_mut(s: &mut Ui) -> Option<&mut Form> {
    match &mut s.sheet {
        Some(Sheet::Connect(f)) => Some(f),
        _ => None,
    }
}

fn protocol_id(p: Protocol) -> String {
    format!("{p:?}")
}

impl Ui {
    pub fn connect_from_form(&mut self) {
        let fallback = self.connect_pane(self.active);
        let Some(form) = form_mut(self) else { return };
        if !form.validate() {
            return;
        }
        let (site, secrets) = form.to_site(None);
        let target = Target { site, pane: form.pane.unwrap_or(fallback), secrets, save: form.save, folder: form.folder.clone(), trusted_once: Vec::new() };
        self.connect(target);
    }

    /// Saves the sheet as a favorite without connecting.
    pub fn save_form(&mut self) {
        let Some(form) = form_mut(self) else { return };
        if !form.validate() {
            return;
        }
        let (site, secrets) = form.to_site(None);
        let folder = form.folder.clone();
        for (account, value) in &secrets {
            if let Err(e) = hub().secrets.set(account, value) {
                self.notify_error(&e);
                return;
            }
        }
        self.sidebar_selected = Some(site.id.clone());
        self.sites.upsert(Favorite { site, folder });
        self.save_sites();
        self.sheet = None;
    }

    fn pick_key_file(&mut self, tx: &mut Tx) {
        let dir = filewinch_os::dirs::Dirs::current().map(|d| d.ssh());
        let opts = FileDialogOptions { title: "Choose a Private Key".into(), directory: dir, ..Default::default() };
        tx.open_file(opts, |s: &mut Ui, res, _tx: &mut Tx| match res {
            Ok(Some(path)) => {
                if let Some(f) = form_mut(s) {
                    f.key_path = rune::TextEdit::new(path.display().to_string());
                    f.errors.remove(&Field::KeyPath);
                }
            }
            Ok(None) => {}
            Err(e) => s.notify(crate::ui::Tone::Warning, "Couldn't show the file picker.", Some(e.to_string())),
        });
    }

    fn sign_in(&mut self) {
        let Some(form) = form_mut(self) else { return };
        let p = form.protocol;
        form.errors.remove(&Field::SignIn);
        match crate::oauth::sign_in_available(p) {
            false => {
                form.errors.insert(Field::SignIn, format!("Signing in to {} isn't available in this build yet.", p.label()));
            }
            true => {
                form.signing_in = true;
                self.run(move || crate::oauth::sign_in(p), |ui, res| {
                    let Some(form) = form_mut(ui) else { return };
                    form.signing_in = false;
                    match res {
                        Ok((token, account)) => {
                            form.oauth_token = Some(token);
                            form.oauth_account = Some(account);
                        }
                        Err(e) => {
                            form.errors.insert(Field::SignIn, e.message);
                        }
                    }
                });
            }
        }
    }
}

fn field_row(k: &Kit, f: &Form, field: Field, control: El) -> El {
    let (label, _) = f.label(field);
    let mut right = col().gap(3.0).grow(1.0).min_w(0.0).child(control);
    if let Some(err) = f.errors.get(&field) {
        right = right.child(text(err.clone()).key(format!("err-{field:?}")).font(k.ty.text_xsmall).color(k.c.danger.fg).live_region(rune::LiveRegion::Polite));
    }
    row().key(format!("row-{field:?}")).w_full().gap(12.0).items_start()
        .child(text(label).font(k.ty.text_small).color(k.c.text_muted).w(124.0).align(rune::Align::End).pt(6.0).a11y_hidden())
        .child(right)
}

fn input(k: &Kit, f: &Form, field: Field) -> El {
    let (label, placeholder) = f.label(field);
    let mut t = TextInput::new(f.edit_ref(field))
        .label(label)
        .placeholder(placeholder)
        .invalid(f.errors.contains_key(&field))
        .on_edit(move |s: &mut Ui| match &mut s.sheet {
            Some(Sheet::Connect(f)) => f.edit_mut(field),
            _ => &mut s.sidebar_search,
        })
        .on_change(move |s: &mut Ui, _tx: &mut Tx| {
            if let Some(f) = form_mut(s) {
                f.errors.remove(&field);
            }
        })
        .on_submit(|s: &mut Ui, _tx: &mut Tx| s.connect_from_form());
    if field == Field::Password {
        t = t.secure();
    }
    t = t.width(if field == Field::Port { Some(90.0) } else { None });
    t.build(k).key(format!("field-{field:?}"))
}

fn sign_in_control(k: &Kit, f: &Form) -> El {
    let p = f.protocol;
    let label = match (&f.oauth_account, f.signing_in) {
        (_, true) => "Waiting for your browser…".to_owned(),
        (Some(_), false) => format!("Sign in to {} Again", p.label()),
        (None, false) => format!("Sign in with {}", p.label()),
    };
    let mut c = row().gap(10.0).center_y().child(
        Button::new(label).variant(ButtonVariant::Neutral).control_size(ControlSize::Regular).disabled(f.signing_in)
            .on_press(|s: &mut Ui| s.sign_in()).build(k).key("sign-in"),
    );
    if f.signing_in {
        c = c.child(Spinner::new().size(14.0).build(k));
    }
    if let Some(a) = &f.oauth_account {
        c = c.child(text(format!("Signed in as {a}")).font(k.ty.text_small).color(k.c.text_muted));
    }
    c
}

fn status(s: &Ui, k: &Kit) -> Option<El> {
    match &s.conn {
        Conn::Connecting { target, .. } => Some(row().key("connecting").gap(8.0).center_y()
            .child(Spinner::new().size(14.0).build(k))
            .child(text(format!("Connecting to {}…", target.site.name)).font(k.ty.text_small).color(k.c.text_muted).live_region(rune::LiveRegion::Polite))),
        _ => None,
    }
}

pub fn sheet(s: &Ui, f: &Form, k: &Kit, paint: &Paint, layer: &mut Vec<El>) {
    let p = f.protocol;
    let busy = matches!(s.conn, Conn::Connecting { .. });
    let picker = row().key("protocol").role("combobox").label(format!("Protocol: {}", p.label())).expanded(false).focusable(true)
        .h(30.0).px(10.0).gap(8.0).center_y().radius(6.0).fill(k.c.surface_raised_base).border(1.0, k.c.border_base).hover(k.c.hover)
        .on_press_tx(|s: &mut Ui, tx: &mut Tx| {
            let Some(f) = form_mut(s) else { return };
            let items: Vec<(String, String)> = registry::ALL.iter().map(|p| (protocol_id(*p), if registry::available(*p) { p.label().to_owned() } else { format!("{} (not in this build)", p.label()) })).collect();
            let refs: Vec<(&str, &str)> = items.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
            let menu = PopupMenu::picker(refs, &protocol_id(f.protocol));
            tx.popup_menu(menu, "protocol", |s: &mut Ui, picked, _tx: &mut Tx| {
                if let (Some(id), Some(f)) = (picked, form_mut(s))
                    && let Some(p) = registry::ALL.iter().find(|p| protocol_id(**p) == id)
                {
                    f.set_protocol(*p);
                }
            });
        })
        .child(icon(icons::protocol(p), 16.0).color(paint.accent).a11y_hidden())
        .child(text(p.label()).font(k.ty.text_small_medium).color(k.c.text_base).a11y_hidden())
        .child(icon(k.glyph.chevron_down, 14.0).color(k.c.icon_muted).a11y_hidden());
    let mut body = col().key("connect-body").gap(10.0).px(16.0).pb(16.0).w_full()
        .child(row().w_full().gap(12.0).center_y()
            .child(text("Protocol").font(k.ty.text_small).color(k.c.text_muted).w(124.0).align(rune::Align::End).a11y_hidden())
            .child(picker));
    if !registry::available(p) {
        body = body.child(row().key("unavailable").gap(8.0).padding(8.0).radius(6.0).fill(k.alpha(paint.warning, 0.1)).center_y()
            .child(icon(&icons::INFO, 14.0).color(paint.warning).a11y_hidden())
            .child(text(format!("This build can save {} favorites but can't connect to them yet.", p.label())).font(k.ty.text_xsmall).color(k.c.text_base).wrap()));
    }
    if p == Protocol::Sftp && !f.suggestions.is_empty() && f.editing.is_none() {
        let mut chips = row().key("suggestions").gap(6.0).flex_wrap();
        for (i, h) in f.suggestions.iter().take(8).enumerate() {
            chips = chips.child(Button::new(h.alias.clone()).variant(ButtonVariant::Ghost).control_size(ControlSize::Small).help(format!("From ~/.ssh/config: {}", h.host))
                .on_press(move |s: &mut Ui| {
                    if let Some(f) = form_mut(s) {
                        let h = f.suggestions[i].clone();
                        f.use_suggestion(&h);
                    }
                }).build(k).key(format!("suggest-{i}")));
        }
        body = body.child(row().w_full().gap(12.0).items_start()
            .child(text("From SSH config").font(k.ty.text_small).color(k.c.text_muted).w(124.0).align(rune::Align::End).pt(4.0))
            .child(chips.grow(1.0).min_w(0.0)));
    }
    for field in f.fields() {
        let control = match field {
            Field::SignIn => sign_in_control(k, f),
            Field::KeyPath => row().gap(6.0).w_full().child(input(k, f, field).grow(1.0))
                .child(Button::new("Choose…").variant(ButtonVariant::Neutral).control_size(ControlSize::Small)
                    .on_press_tx(|s: &mut Ui, tx: &mut Tx| s.pick_key_file(tx)).build(k).key("choose-key")),
            _ => input(k, f, field),
        };
        body = body.child(field_row(k, f, field, control));
        if field == Field::User && p == Protocol::Sftp {
            let current = match f.auth {
                AuthChoice::Password => 0,
                AuthChoice::KeyFile => 1,
                AuthChoice::Agent => 2,
            };
            let seg = SegmentedControl::new(&["Password", "Key File", "SSH Agent"], Some(current)).label("Authentication").fit()
                .on_pick(|s: &mut Ui, i, _tx: &mut Tx| {
                    if let Some(f) = form_mut(s) {
                        f.auth = [AuthChoice::Password, AuthChoice::KeyFile, AuthChoice::Agent][i.min(2)];
                        f.errors.clear();
                    }
                }).build(k).key("auth");
            body = body.child(row().w_full().gap(12.0).center_y()
                .child(text("Log in with").font(k.ty.text_small).color(k.c.text_muted).w(124.0).align(rune::Align::End).a11y_hidden())
                .child(seg));
        }
    }
    let mut extras = col().gap(6.0).pl(136.0);
    if p == Protocol::Ftps {
        let seg = SegmentedControl::new(&["Explicit (port 21)", "Implicit (port 990)"], Some(if f.tls == TlsMode::Implicit { 1 } else { 0 })).label("TLS mode").fit()
            .on_pick(|s: &mut Ui, i, _tx: &mut Tx| {
                if let Some(f) = form_mut(s) {
                    f.tls = if i == 1 { TlsMode::Implicit } else { TlsMode::Explicit };
                }
            }).build(k).key("tls");
        extras = extras.child(seg);
    }
    if matches!(p, Protocol::Ftp | Protocol::Ftps) {
        extras = extras.child(Checkbox::new(f.passive).label("Passive mode (works through most firewalls)")
            .on_toggle(|s: &mut Ui| if let Some(f) = form_mut(s) { f.passive = !f.passive }).build(k).key("passive"));
    }
    if p == Protocol::S3 {
        extras = extras.child(Checkbox::new(f.path_style).label("Path-style addressing (MinIO and similar)")
            .on_toggle(|s: &mut Ui| if let Some(f) = form_mut(s) { f.path_style = !f.path_style }).build(k).key("path-style"));
    }
    let save = Checkbox::new(f.save).label(if f.editing.is_some() { "Keep in Favorites" } else { "Save as favorite" })
        .disabled(f.editing.is_some())
        .on_toggle(|s: &mut Ui| if let Some(f) = form_mut(s) { f.save = !f.save }).build(k).key("save-favorite");
    extras = extras.child(save);
    body = body.child(extras);
    let mut footer = row().w_full().gap(8.0).center_y().pt(4.0);
    footer = match status(s, k) {
        Some(st) => footer.child(st.grow(1.0)),
        None => footer.child(rune::el().grow(1.0)),
    };
    if busy {
        footer = footer.child(Button::new("Cancel").variant(ButtonVariant::Neutral).on_press(|s: &mut Ui| s.cancel_connect()).build(k).key("cancel-connect"));
    } else {
        footer = footer.child(Button::new("Cancel").variant(ButtonVariant::Ghost).on_press(|s: &mut Ui| {
            crate::oauth::cancel_sign_in();
            s.sheet = None;
        }).build(k).key("cancel"));
        if f.editing.is_some() || f.save {
            footer = footer.child(Button::new("Save").variant(ButtonVariant::Neutral).on_press(|s: &mut Ui| s.save_form()).build(k).key("save"));
        }
        footer = footer.child(Button::new("Connect").variant(ButtonVariant::Contrast)
            .on_press(|s: &mut Ui| s.connect_from_form()).build(&crate::theme::primary(k)).key("connect-submit").default_button());
    }
    body = body.child(footer);
    let title = if f.editing.is_some() { "Edit Favorite" } else { "Connect to a Server" };
    Dialog::new("connect", title).open(true).width(580.0).fit().child(body).click_outside(false)
        .on_close(|s: &mut Ui| {
            crate::oauth::cancel_sign_in();
            s.cancel_connect();
            s.sheet = None;
        })
        .build(k, layer);
}

/// The connection's own dialog: shown on its own for a favorite, or over the connect sheet
/// for a failure, password or trust question.
pub fn conn_dialog(s: &Ui, k: &Kit, paint: &Paint, layer: &mut Vec<El>) {
    let form_open = matches!(s.sheet, Some(Sheet::Connect(_)));
    let body = |children: Vec<El>| {
        let mut c = col().key("conn-body").gap(12.0).px(16.0).pb(16.0).w_full();
        for ch in children {
            c = c.child(ch);
        }
        c
    };
    let buttons = |items: Vec<El>| {
        let mut r = row().w_full().gap(8.0).child(rune::el().grow(1.0));
        for i in items {
            r = r.child(i);
        }
        r
    };
    match &s.conn {
        Conn::Idle => {}
        Conn::Connecting { target, .. } if !form_open => {
            let b = body(vec![
                row().gap(10.0).center_y().child(Spinner::new().size(16.0).build(k))
                    .child(text(format!("Connecting to {}…", if target.site.host.is_empty() { target.site.protocol.label() } else { &target.site.host }))
                        .font(k.ty.text_small).color(k.c.text_muted).live_region(rune::LiveRegion::Polite)),
                buttons(vec![Button::new("Cancel").variant(ButtonVariant::Neutral).on_press(|s: &mut Ui| s.cancel_connect()).build(k).key("cancel-connect")]),
            ]);
            Dialog::new("conn", format!("Connecting to {}", target.site.name)).open(true).width(420.0).fit().child(b)
                .on_close(|s: &mut Ui| s.cancel_connect()).build(k, layer);
        }
        Conn::Connecting { .. } => {}
        Conn::Failed { target, error, details } => {
            let mut parts = vec![row().gap(10.0).items_start()
                .child(icon(&icons::WARNING, 18.0).color(paint.danger).a11y_hidden())
                .child(text(error.message.clone()).key("conn-error").font(k.ty.text_small).color(k.c.text_base).wrap().grow(1.0).role("alert"))];
            if let Some(d) = &error.detail {
                let open = *details;
                parts.push(row().key("details-toggle").role("button").label(if open { "Hide details" } else { "Show details" }).expanded(open).focusable(true)
                    .gap(4.0).center_y().h(22.0).radius(4.0).hover(k.c.hover)
                    .on_press(|s: &mut Ui| if let Conn::Failed { details, .. } = &mut s.conn { *details = !*details })
                    .child(icon(if open { k.glyph.chevron_down } else { &icons::FORWARD }, 12.0).color(k.c.icon_muted).a11y_hidden())
                    .child(text("Details").font(k.ty.text_small_medium).color(k.c.text_muted).a11y_hidden()));
                if open {
                    parts.push(text(d.clone()).key("details").font(k.ty.mono).color(k.c.text_muted).wrap().selectable(true).padding(8.0).radius(6.0).fill(paint.chrome).w_full());
                }
            }
            parts.push(buttons(vec![
                Button::new(if form_open { "Edit Settings" } else { "Close" }).variant(ButtonVariant::Ghost).on_press(|s: &mut Ui| s.cancel_connect()).build(k).key("close-error"),
                Button::new("Try Again").variant(ButtonVariant::Contrast).on_press(|s: &mut Ui| s.retry_connect()).build(&crate::theme::primary(k)).key("retry-connect").default_button(),
            ]));
            Dialog::new("conn", format!("Couldn't connect to {}", target.site.name)).open(true).width(460.0).fit().child(body(parts))
                .on_close(|s: &mut Ui| s.cancel_connect()).build(k, layer);
        }
        Conn::Password { target, edit, message } => {
            let field = TextInput::new(edit).label("Password").placeholder("Password").secure()
                .on_edit(|s: &mut Ui| match &mut s.conn {
                    Conn::Password { edit, .. } => edit,
                    _ => &mut s.sidebar_search,
                })
                .on_submit(|s: &mut Ui, _tx: &mut Tx| s.submit_password())
                .build(k).key("password-again");
            let who = if target.site.user.is_empty() { target.site.host.clone() } else { format!("{}@{}", target.site.user, target.site.host) };
            let b = body(vec![
                text(message.clone()).key("auth-message").font(k.ty.text_small).color(k.c.text_base).wrap().role("alert"),
                text(format!("Enter the password for {who}. It's saved in {} if this site is a favorite.", crate::theme::ui_os().secret_store())).font(k.ty.text_xsmall).color(k.c.text_muted).wrap(),
                field,
                buttons(vec![
                    Button::new("Cancel").variant(ButtonVariant::Ghost).on_press(|s: &mut Ui| s.cancel_connect()).build(k).key("cancel-password"),
                    Button::new("Connect").variant(ButtonVariant::Contrast).on_press(|s: &mut Ui| s.submit_password()).build(&crate::theme::primary(k)).key("submit-password").default_button(),
                ]),
            ]);
            Dialog::new("conn", format!("Log in to {}", target.site.name)).open(true).width(420.0).fit().child(b)
                .on_close(|s: &mut Ui| s.cancel_connect()).build(k, layer);
        }
        Conn::Trust { target, fingerprint, reason } => {
            let b = body(vec![
                row().gap(10.0).items_start()
                    .child(icon(&icons::LOCK, 18.0).color(paint.warning).a11y_hidden())
                    .child(text(format!("Filewinch can't confirm that this is really {}. {reason}", target.site.host)).font(k.ty.text_small).color(k.c.text_base).wrap().grow(1.0)),
                col().gap(4.0).w_full()
                    .child(text("Fingerprint").font(k.ty.text_xsmall_medium).color(k.c.text_muted))
                    .child(text(if fingerprint.is_empty() { "The server didn't give one.".to_owned() } else { fingerprint.clone() }).key("fingerprint").font(k.ty.mono).color(k.c.text_base).wrap().selectable(true).padding(8.0).radius(6.0).fill(paint.chrome).w_full()),
                text("Compare it with the one your server's administrator gave you. Only trust it if they match.").font(k.ty.text_xsmall).color(k.c.text_muted).wrap(),
                buttons(vec![
                    Button::new("Cancel").variant(ButtonVariant::Ghost).on_press(|s: &mut Ui| s.cancel_connect()).build(k).key("trust-cancel"),
                    Button::new("Trust Once").variant(ButtonVariant::Neutral).on_press(|s: &mut Ui| s.trust(false)).build(k).key("trust-once"),
                    Button::new("Always Trust").variant(ButtonVariant::Contrast).on_press(|s: &mut Ui| s.trust(true)).build(&crate::theme::primary(k)).key("trust-always"),
                ]),
            ]);
            Dialog::new("conn", "Unknown Server Identity").open(true).width(500.0).fit().child(b)
                .on_close(|s: &mut Ui| s.cancel_connect()).build(k, layer);
        }
    }
}
