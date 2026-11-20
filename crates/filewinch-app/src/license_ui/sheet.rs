//! Enter Licence Key: a paste field with the key's format, Activate with progress, and the
//! error inline (with the customer portal one click away when that's where the fix is).

use filewinch_license::PORTAL_URL;
use rune::{El, Tx, col, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, Dialog, Kit, Spinner, TextInput};

use super::{LicenseSheet, Phase};
use crate::theme::{self, Paint};
use crate::ui::{Sheet, Ui};

pub const PLACEHOLDER: &str = "FW-XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX";

pub fn view(l: &LicenseSheet, k: &Kit, paint: &Paint, layer: &mut Vec<El>) {
    let busy = l.phase == Phase::Activating;
    let error = l.error();
    let field = TextInput::new(&l.key).label("Licence key").placeholder(PLACEHOLDER).width(None).invalid(error.is_some()).disabled(busy)
        .on_edit(|s: &mut Ui| match &mut s.sheet {
            Some(Sheet::License(l)) => {
                // Typing again clears the last error.
                if matches!(l.phase, Phase::Invalid(_) | Phase::Failed(_)) {
                    l.phase = Phase::Idle;
                }
                &mut l.key
            }
            _ => &mut s.sidebar_search,
        })
        .on_submit(|s: &mut Ui, _tx: &mut Tx| s.activate_license()).build(k).key("license-key");
    let mut body = col().key("license-body").gap(12.0).px(16.0).pb(16.0).w_full()
        .child(text("Paste the licence key from your receipt email. It starts with FW-.").key("license-hint").font(k.ty.text_small).color(k.c.text_muted).wrap())
        .child(field);
    if let Some(e) = error {
        let mut line = col().key("license-error").gap(6.0).w_full()
            .child(text(e).font(k.ty.text_xsmall).color(paint.danger).wrap().live_region(rune::LiveRegion::Polite));
        if let Phase::Failed(f) = &l.phase
            && f.wants_portal()
        {
            line = line.child(row().w_full().child(Button::new("Open Customer Portal").variant(ButtonVariant::Ghost).control_size(ControlSize::Small).expanded(false)
                .on_press_tx(|s: &mut Ui, tx: &mut Tx| open(s, tx, PORTAL_URL)).build(k).key("license-portal")).child(rune::el().grow(1.0)));
        }
        body = body.child(line);
    }
    let mut activate = Button::new(if busy { "Activating…" } else { "Activate" }).variant(ButtonVariant::Contrast).disabled(busy)
        .on_press(|s: &mut Ui| s.activate_license()).build(&theme::primary(k)).key("license-activate");
    if !busy {
        activate = activate.default_button();
    }
    let mut buttons = row().w_full().gap(8.0).center_y()
        .child(Button::new("Buy a Licence…").variant(ButtonVariant::Ghost).control_size(ControlSize::Small)
            .on_press_tx(|s: &mut Ui, tx: &mut Tx| super::banner::buy(s, tx)).build(k).key("license-buy"))
        .child(rune::el().grow(1.0));
    if busy {
        buttons = buttons.child(Spinner::new().size(14.0).build(k).label("Activating"));
    }
    buttons = buttons
        .child(Button::new("Cancel").variant(ButtonVariant::Ghost).on_press(|s: &mut Ui| s.sheet = None).build(k).key("license-cancel"))
        .child(activate);
    body = body.child(buttons);
    Dialog::new("license", "Enter Licence Key").open(true).width(460.0).fit().child(body).on_close(|s: &mut Ui| s.sheet = None).build(k, layer);
}

fn open(s: &mut Ui, tx: &mut Tx, url: &str) {
    if let Err(e) = tx.open_url(url) {
        s.notify(crate::ui::Tone::Error, format!("Filewinch couldn't open your browser. Go to {url}"), Some(e.to_string()));
    }
}
