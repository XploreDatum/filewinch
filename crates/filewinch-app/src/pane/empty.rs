//! A pane that shows nothing yet (the right pane at launch, or after Disconnect): a centred
//! invitation to connect to a server, or to browse This Mac instead.

use rune::{Align, El, Key, KeyEvent, Mods, Tx, col, icon, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit};

use crate::icons;
use crate::theme::Paint;
use crate::ui::Ui;

/// The empty pane's body, in place of the path bar and the file table. It takes the pane's
/// focus key (`pane-{p}`), so Cmd-1/2 and Tab from the other pane land here, and Return
/// connects (Space presses whichever button has focus). There's no drop target: files dragged
/// over it get the system's "can't drop here".
pub fn body(p: usize, k: &Kit, paint: &Paint) -> El {
    let connect = Button::new("Connect…").icon(&icons::SERVER).variant(ButtonVariant::Contrast).control_size(ControlSize::Regular)
        .help(format!("Connect to a server in this pane ({})", crate::keys::Act::Connect.label()))
        .on_press(move |s: &mut Ui| s.open_connect_in(p)).build(&crate::theme::primary(k)).key(format!("empty-connect-{p}"));
    let local = Button::new(crate::theme::ui_os().this_computer()).icon(&icons::LAPTOP).variant(ButtonVariant::Neutral).control_size(ControlSize::Regular)
        .help("Browse your home folder in this pane")
        .on_press(move |s: &mut Ui| s.show_local(p)).build(k).key(format!("empty-local-{p}"));
    let badge = row().size(48.0, 48.0).center().radius(12.0).fill(k.alpha(paint.accent, if paint.dark { 0.18 } else { 0.10 }))
        .child(icon(&icons::SERVER, 24.0).color(paint.accent)).a11y_hidden();
    col().key(format!("pane-{p}")).role("group").label(format!("Connect to a server, or browse another folder on {}", this_computer_lower()))
        .focusable(true)
        .w_full().grow(1.0).min_h(0.0).center().gap(6.0).padding(24.0)
        .on_press(move |s: &mut Ui| s.active = p)
        .on_key(move |s: &mut Ui, ev: &KeyEvent, _tx: &mut Tx| s.empty_pane_key(p, ev))
        .child(badge.mb(8.0))
        .child(text("Connect to a server").key("empty-title").role("heading").level(2).font(k.ty.ui_medium).color(k.c.text_strong))
        .child(text(format!("Or browse another folder on {}.", this_computer_lower())).font(k.ty.text_small).color(k.c.text_muted).wrap().max_w(320.0).align(Align::Center))
        .child(row().key("empty-actions").gap(8.0).mt(12.0).child(connect).child(local))
}

impl Ui {
    /// Keys while an empty pane (or one of its buttons) has focus: Return connects, as the
    /// primary button; Tab and Space keep their usual focus moves and presses.
    pub fn empty_pane_key(&mut self, p: usize, ev: &KeyEvent) -> bool {
        self.active = p;
        if ev.key == Key::Enter && ev.mods == Mods::default() {
            self.open_connect_in(p);
            return true;
        }
        false
    }
}

/// "this Mac" / "this computer", mid-sentence.
fn this_computer_lower() -> &'static str {
    match crate::theme::ui_os() {
        filewinch_os::Os::Mac => "this Mac",
        _ => "this computer",
    }
}
