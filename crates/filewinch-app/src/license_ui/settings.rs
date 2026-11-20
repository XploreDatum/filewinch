//! Settings ▸ Licence: the status, the masked key, Deactivate This Device, and Buy / Enter
//! Licence Key for an unregistered copy.

use filewinch_license::{PRICE, Status};
use rune::{El, Tx, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit, Spinner};

use crate::hub::hub;
use crate::settings::{SettingsUi, group, setting, update};

pub fn group_view(s: &SettingsUi, k: &Kit) -> El {
    group(k, "license", "Licence", rows(s, k))
}

fn button(k: &Kit, label: &str, key: &str, variant: ButtonVariant, f: impl Fn(&mut SettingsUi, &mut Tx) + 'static) -> El {
    Button::new(label).variant(variant).control_size(ControlSize::Small).expanded(false).on_press_tx(f).build(k).key(key.to_owned())
}

fn rows(s: &SettingsUi, k: &Kit) -> Vec<El> {
    let mut rows = match &s.license {
        Status::Unregistered => vec![
            setting(k, "Unregistered", Some(&format!("Filewinch is free to use with every feature. A licence ({PRICE}, one time) supports development and removes the banner.")), rune::el()),
            row().w_full().gap(8.0).px(14.0).py(8.0).center_y()
                .child(rune::el().grow(1.0))
                .child(button(k, "Enter Licence Key…", "license-enter", ButtonVariant::Neutral, |_, tx| enter_key(tx)))
                .child(button(k, "Buy a Licence…", "license-buy", ButtonVariant::Neutral, |s, tx| open(s, tx, filewinch_license::CHECKOUT_URL))),
        ],
        Status::Registered(r) => {
            let device = if s.license_busy {
                row().gap(8.0).center_y().child(Spinner::new().size(14.0).build(k).label("Deactivating"))
                    .child(text("Deactivating…").font(k.ty.text_small).color(k.c.text_muted))
            } else if s.confirm_deactivate {
                row().gap(8.0).center_y()
                    .child(button(k, "Cancel", "deactivate-cancel", ButtonVariant::Ghost, |s, _| s.confirm_deactivate = false))
                    .child(button(k, "Deactivate", "deactivate-ok", ButtonVariant::Danger, |s, _| deactivate(s)))
            } else {
                button(k, "Deactivate This Device…", "license-deactivate", ButtonVariant::Neutral, |s, _| {
                    s.confirm_deactivate = true;
                    s.license_error = None;
                })
            };
            let detail = if s.confirm_deactivate {
                "Free this device's slot? Filewinch keeps working, and the banner comes back until you enter the key again.".to_owned()
            } else if r.label.is_empty() {
                "This computer".to_owned()
            } else {
                r.label.clone()
            };
            vec![
                setting(k, &crate::license_ui::status_line(&s.license), Some("Thank you for supporting Filewinch."), rune::el()),
                setting(k, "Licence key", None, text(r.masked_key.clone()).key("license-masked").font(k.ty.mono).color(k.c.text_muted).selectable(true)),
                setting(k, "This device", Some(&detail), device),
            ]
        }
    };
    if let Some(e) = &s.license_error {
        rows.push(row().w_full().px(14.0).py(8.0).child(text(e.clone()).key("license-error").font(k.ty.text_xsmall).color(k.c.danger.fg).wrap().role("alert")));
    }
    rows
}

fn open(s: &mut SettingsUi, tx: &mut Tx, url: &str) {
    s.license_error = tx.open_url(url).err().map(|e| format!("Filewinch couldn't open your browser ({e}). The address is {url}"));
}

/// Closes Settings and opens the Enter Licence Key sheet in the main window (a new one if
/// none is open).
fn enter_key(tx: &mut Tx) {
    tx.close_window();
    super::request_sheet();
}

fn deactivate(s: &mut SettingsUi) {
    s.license_busy = true;
    s.confirm_deactivate = false;
    s.license_error = None;
    let spawned = std::thread::Builder::new().name("filewinch-deactivate".into()).spawn(|| match hub().licensing.deactivate() {
        Ok(()) => super::broadcast(Status::Unregistered),
        Err(f) => update(move |s| {
            s.license_busy = false;
            s.license_error = Some(format!("Filewinch couldn't deactivate this device. {}", f.message()));
        }),
    });
    if spawned.is_err() {
        s.license_busy = false;
        s.license_error = Some("Filewinch couldn't start deactivating. Try again.".into());
    }
}
