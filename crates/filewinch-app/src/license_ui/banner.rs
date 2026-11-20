//! The one reminder an unregistered Filewinch shows: a slim bar above the Transfers drawer
//! (or at the window's bottom when the drawer is hidden). It sits in the layout, so it never
//! covers anything; it doesn't move, blink or pop anything up. Buying is what removes it.

use filewinch_license::{CHECKOUT_URL, PRICE};
use rune::{El, Tx, icon, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit};

use crate::icons;
use crate::theme::Paint;
use crate::ui::{Tone, Ui};

/// The banner's height, for anything pinned above the window's bottom edge.
pub const HEIGHT: f32 = 36.0;

pub fn message() -> String {
    format!("Filewinch is free to use. A licence ({PRICE}) supports development and removes this banner.")
}

/// The banner, or `None` once registered.
pub fn view(s: &Ui, k: &Kit, paint: &Paint) -> Option<El> {
    if s.license.is_registered() {
        return None;
    }
    let buy = Button::new("Buy a Licence…").variant(ButtonVariant::Neutral).control_size(ControlSize::Small)
        .on_press_tx(|s: &mut Ui, tx: &mut Tx| buy(s, tx)).build(k).key("banner-buy");
    let enter = Button::new("Enter Licence Key…").variant(ButtonVariant::Ghost).control_size(ControlSize::Small)
        .on_press(|s: &mut Ui| s.open_license_sheet()).build(k).key("banner-enter");
    Some(
        row().key("license-banner").role("region").label("Licence").w_full().h(HEIGHT).px(12.0).gap(10.0).center_y()
            .fill(k.alpha(paint.accent, if paint.dark { 0.10 } else { 0.06 }))
            .border_sides([1.0, 0.0, 0.0, 0.0], paint.hair)
            .child(icon(&icons::WINCH, 14.0).color(paint.accent).a11y_hidden())
            .child(text(message()).key("banner-text").font(k.ty.text_small).color(k.c.text_base).grow(1.0).min_w(0.0).truncate())
            .child(enter)
            .child(buy),
    )
}

/// Opens the checkout in the browser; if the system refuses, says where to go instead.
pub fn buy(s: &mut Ui, tx: &mut Tx) {
    if let Err(e) = tx.open_url(CHECKOUT_URL) {
        s.notify(Tone::Error, format!("Filewinch couldn't open your browser. Buy a licence at {CHECKOUT_URL}"), Some(e.to_string()));
    }
}
