//! The banner (shown only unregistered, never covering anything, its buttons), the Enter
//! Licence Key sheet's states (format check, activating, each error, success), and the
//! revoked notice.

use filewinch_license::{CHECKOUT_URL, Failure, PORTAL_URL, Status};
use rune::{Env, Runtime};

use super::{LicenseSheet, Phase, REVOKED, sample};
use crate::ui::{Sheet, Ui};

fn unregistered() -> Ui {
    let mut ui = Ui::new();
    ui.license = Status::Unregistered;
    ui
}

fn runtime(ui: Ui) -> Runtime<Ui> {
    let mut rt = Runtime::new(ui, crate::view::view, (1320.0, 820.0), Env::new(false, 1.0));
    rt.frame(0.0);
    rt
}

fn click(rt: &mut Runtime<Ui>, key: &str) {
    let (x, y) = rt.find(key).unwrap_or_else(|| panic!("{key} is on screen"));
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(1.0 / 60.0);
}

fn sheet(ui: &Ui) -> &LicenseSheet {
    match &ui.sheet {
        Some(Sheet::License(l)) => l,
        _ => panic!("the licence sheet is open"),
    }
}

fn sheet_mut(ui: &mut Ui) -> &mut LicenseSheet {
    match &mut ui.sheet {
        Some(Sheet::License(l)) => l,
        _ => panic!("the licence sheet is open"),
    }
}

#[test]
fn the_banner_shows_only_while_unregistered() {
    let mut rt = runtime(unregistered());
    assert!(rt.find("license-banner").is_some());
    assert!(super::banner::message().contains("$14"));
    rt.update(|s, _| s.license = Status::Registered(sample()));
    rt.frame(1.0 / 60.0);
    assert!(rt.find("license-banner").is_none(), "buying removes it");
    assert!(rt.find("banner-buy").is_none());
}

#[test]
fn the_banner_sits_in_the_layout_above_the_transfers() {
    let mut ui = unregistered();
    ui.prefs.transfers_open = true;
    let rt = runtime(ui);
    let (_, banner_y) = rt.find("license-banner").expect("banner");
    let (_, drawer_y) = rt.find("hide-transfers").expect("drawer");
    let (_, panes_y) = rt.find("panes").expect("panes");
    assert!(panes_y < banner_y && banner_y < drawer_y, "panes {panes_y} < banner {banner_y} < drawer {drawer_y}");
    // It's a row in the column, not an overlay: what's under its centre is the banner itself.
    let (x, y) = rt.find("banner-text").expect("text");
    let picked = rt.element_at(x, y).expect("something");
    assert!(picked.path().contains("license-banner"), "{}", picked.path());
}

#[test]
fn buy_opens_the_checkout_and_enter_opens_the_sheet() {
    let mut rt = runtime(unregistered());
    click(&mut rt, "banner-buy");
    assert_eq!(rt.take_url_requests(), vec![CHECKOUT_URL.to_owned()]);
    assert!(rt.state.sheet.is_none(), "buying opens no dialog");
    click(&mut rt, "banner-enter");
    assert_eq!(sheet(&rt.state).phase, Phase::Idle);
    assert!(rt.find("license-key").is_some());
    assert!(rt.find("license-activate").is_some());
}

#[test]
fn a_mistyped_key_is_caught_before_the_network() {
    let mut ui = unregistered();
    ui.open_license_sheet();
    ui.activate_license();
    assert_eq!(sheet(&ui).phase, Phase::Invalid("Paste the licence key from your receipt email."));
    sheet_mut(&mut ui).key.text = "1C285B2D-6CE6-4BC7-B8BE-ADB6A7E304DA".into();
    ui.activate_license();
    assert!(sheet(&ui).error().unwrap().contains("start with FW-"));
    let rt = runtime(ui);
    assert!(rt.find("license-error").is_some(), "the error is inline");
}

#[test]
fn activating_shows_progress_then_registers() {
    let mut ui = unregistered();
    ui.open_license_sheet();
    sheet_mut(&mut ui).key.text = "  FW-1C285B2D-6CE6-4BC7-B8BE-ADB6A7E304DA \n".into();
    ui.activate_license();
    assert_eq!(sheet(&ui).phase, Phase::Activating);
    // A second press while busy does nothing.
    ui.activate_license();
    assert_eq!(sheet(&ui).phase, Phase::Activating);
    let mut rt = runtime(ui);
    assert!(rt.find("license-activate").is_some(), "the button stays, disabled, reading Activating…");
    // Polar's answer arrives (the test hub's client is offline, so hand it in).
    rt.update(|s, _| s.license_activated(Ok(sample())));
    rt.frame(1.0 / 60.0);
    assert!(rt.state.sheet.is_none());
    assert!(rt.state.license.is_registered());
    assert!(rt.find("license-banner").is_none());
    assert!(rt.state.notices.iter().any(|n| n.text.contains("Thank you") && n.text.contains("Alex Morgan")));
}

#[test]
fn each_failure_reads_plainly_inline() {
    let cases = [
        (Failure::WrongKey, "wasn't recognised", false),
        (Failure::LimitReached, "5 devices", true),
        (Failure::NotActive("License key is no longer active.".into()), "no longer active", true),
        (Failure::Offline("dns".into()), "Couldn't reach the licence server. Try again later.", false),
        (Failure::Server("HTTP 503".into()), "Couldn't reach the licence server. Try again later.", false),
    ];
    for (failure, says, portal) in cases {
        let mut ui = unregistered();
        ui.open_license_sheet();
        sheet_mut(&mut ui).phase = Phase::Activating;
        ui.license_activated(Err(failure.clone()));
        assert!(sheet(&ui).error().unwrap().contains(says), "{failure:?}");
        assert!(!ui.license.is_registered());
        let mut rt = runtime(ui);
        assert!(rt.find("license-error").is_some());
        assert_eq!(rt.find("license-portal").is_some(), portal, "{failure:?}");
        if portal {
            click(&mut rt, "license-portal");
            assert_eq!(rt.take_url_requests(), vec![PORTAL_URL.to_owned()]);
        }
    }
}

#[test]
fn typing_again_clears_the_error() {
    let mut ui = unregistered();
    ui.open_license_sheet();
    sheet_mut(&mut ui).phase = Phase::Failed(Failure::WrongKey);
    let mut rt = runtime(ui);
    click(&mut rt, "license-key");
    rt.key(rune::KeyEvent { key: rune::Key::Text("F".into()), mods: rune::Mods::default() });
    rt.frame(1.0 / 60.0);
    assert_eq!(sheet(&rt.state).phase, Phase::Idle);
}

#[test]
fn an_answer_after_the_sheet_closed_still_counts() {
    let mut ui = unregistered();
    ui.license_activated(Ok(sample()));
    assert!(ui.license.is_registered());
    let mut ui = unregistered();
    ui.license_activated(Err(Failure::WrongKey));
    assert!(ui.notices.iter().any(|n| n.text.contains("couldn't activate")));
}

#[test]
fn a_revoked_licence_brings_the_banner_back_calmly() {
    let mut rt = runtime(Ui::new());
    assert!(rt.find("license-banner").is_none());
    rt.update(|s, _| s.licence_revoked());
    rt.frame(1.0 / 60.0);
    assert!(rt.find("license-banner").is_some());
    let n = rt.state.notices.last().expect("a notice");
    assert_eq!(n.text, REVOKED);
    assert_eq!(n.until, None, "it stays until read");
    assert!(rt.state.sheet.is_none(), "no dialog");
}
