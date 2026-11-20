//! About Filewinch: its own window (Filewinch ▸ About on macOS, Help ▸ About in the menu
//! elsewhere) with the version, copyright, the AGPL notice and a link to the source, the
//! licence status, and the third-party notices.

mod notices;
#[cfg(test)]
mod tests;

use std::sync::{Mutex, OnceLock};
use std::sync::atomic::AtomicU64;

use filewinch_license::Status;
use filewinch_store::Appearance;
use rune::{AppHandle, AppSpec, Cx, El, KeyEvent, Tx, Waker, WindowHandle, col, icon, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit};

use crate::hub::hub;
use crate::icons;
use crate::theme;
use crate::ui::Ui;

pub const SOURCE_URL: &str = "https://github.com/XploreDatum/filewinch";
pub const AGPL_URL: &str = "https://www.gnu.org/licenses/agpl-3.0.html";
pub const COPYRIGHT: &str = "Copyright © 2026 XploreDatum Technologies LLP";
pub const LICENSED: &str = "Licensed under the GNU AGPL v3 or later.";
/// The app icon's master (`assets/icon/icon.svg`). Its tile fills 824 of its 1024 units, so at
/// `APP_ICON_PT` the tile shows 72 pt wide, with the rest left for the tile's drop shadow.
const APP_ICON: &[u8] = include_bytes!("../../../../assets/icon/icon.svg");
const APP_ICON_PT: f32 = 90.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    About,
    Notices,
}

pub struct AboutUi {
    pub license: Status,
    pub appearance: Appearance,
    pub page: Page,
    /// The licence text shown in full on the notices page.
    pub open: Option<usize>,
    pub error: Option<String>,
}

impl AboutUi {
    pub fn new() -> AboutUi {
        let appearance = hub().store.as_ref().map(|s| s.load_prefs().value.appearance).unwrap_or_default();
        AboutUi { license: crate::license_ui::initial(), appearance, page: Page::About, open: None, error: None }
    }
}

static WINDOW: OnceLock<(AppHandle<AboutUi>, WindowHandle)> = OnceLock::new();

/// Registers the About window built (hidden) at startup.
pub fn install(handle: AppHandle<AboutUi>, window: WindowHandle) {
    let _ = WINDOW.set((handle, window));
}

/// Shows About, in `s`'s appearance and with its licence status.
pub fn open_about(s: &Ui) {
    let Some((h, w)) = WINDOW.get() else { return };
    let (license, appearance) = (s.license.clone(), s.prefs.appearance);
    h.update(move |a, _| {
        a.license = license;
        a.appearance = appearance;
        a.page = Page::About;
        a.error = None;
    });
    w.open();
    w.focus();
}

/// Keeps an open About window's licence status current.
pub fn push_license(status: Status) {
    if let Some((h, _)) = WINDOW.get() {
        h.update(move |a, _| a.license = status);
    }
}

/// Hides AppKit's standard "About Filewinch" item (it opens the bare system panel), so the
/// app menu's first item is Filewinch's own. Rune has no way to retarget it; hiding keeps the
/// item where Rune expects it, so Rune's own menu placement is unchanged. Main thread.
#[cfg(target_os = "macos")]
pub fn hide_standard_about_item() {
    use objc2::runtime::{AnyClass, AnyObject, Bool, Sel};
    use objc2::{msg_send, sel};
    // SAFETY: called on the main thread from `on_ready`; every object is checked for nil
    // before use and only standard NSApplication / NSMenu / NSMenuItem messages are sent.
    unsafe {
        let Some(class) = AnyClass::get("NSApplication") else { return };
        let app: *mut AnyObject = msg_send![class, sharedApplication];
        if app.is_null() {
            return;
        }
        let bar: *mut AnyObject = msg_send![app, mainMenu];
        if bar.is_null() {
            return;
        }
        let tops: isize = msg_send![bar, numberOfItems];
        if tops == 0 {
            return;
        }
        let top: *mut AnyObject = msg_send![bar, itemAtIndex: 0isize];
        let menu: *mut AnyObject = if top.is_null() { std::ptr::null_mut() } else { msg_send![top, submenu] };
        if menu.is_null() {
            return;
        }
        let n: isize = msg_send![menu, numberOfItems];
        for i in 0..n {
            let item: *mut AnyObject = msg_send![menu, itemAtIndex: i];
            if item.is_null() {
                continue;
            }
            let action: Option<Sel> = msg_send![item, action];
            if action == Some(sel!(orderFrontStandardAboutPanel:)) {
                let _: () = msg_send![item, setHidden: Bool::YES];
            }
        }
    }
}

fn link(s: &mut AboutUi, tx: &mut Tx, url: &str) {
    s.error = tx.open_url(url).err().map(|e| format!("Filewinch couldn't open your browser ({e}). The address is {url}"));
}

/// The app icon at `scale` device pixels per point, rasterized on the spot (so the first frame
/// and headless snapshots have it) and kept until the window moves to a display of another scale.
fn app_icon(scale: f32) -> Option<rune::Pixels> {
    static CACHE: Mutex<Option<(u32, rune::Pixels)>> = Mutex::new(None);
    let px = (APP_ICON_PT * scale).round().max(1.0) as u32;
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, pixels)) = cache.as_ref().filter(|(size, _)| *size == px) {
        return Some(pixels.clone());
    }
    let rgba = rune::image::svg_rgba_cover(APP_ICON, px).ok()?;
    let pixels = rune::Pixels::new();
    pixels.set(px, px, rgba);
    *cache = Some((px, pixels.clone()));
    Some(pixels)
}

fn about_page(s: &AboutUi, k: &Kit, scale: f32) -> El {
    let paint = theme::paint(k);
    let mut tile = rune::el().key("about-icon").w(APP_ICON_PT).h(APP_ICON_PT);
    if let Some(pixels) = app_icon(scale) {
        tile = tile.pixels(&pixels);
    }
    let status_color = if s.license.is_registered() { k.c.text_base } else { k.c.text_muted };
    let buttons = row().gap(8.0).center_y()
        .child(Button::new("Source Code").variant(ButtonVariant::Neutral).control_size(ControlSize::Small)
            .on_press_tx(|s: &mut AboutUi, tx: &mut Tx| link(s, tx, SOURCE_URL)).build(k).key("about-source"))
        .child(Button::new("Licence").variant(ButtonVariant::Neutral).control_size(ControlSize::Small)
            .on_press_tx(|s: &mut AboutUi, tx: &mut Tx| link(s, tx, AGPL_URL)).build(k).key("about-agpl"))
        .child(Button::new("Third-Party Notices").variant(ButtonVariant::Neutral).control_size(ControlSize::Small)
            .on_press(|s: &mut AboutUi| s.page = Page::Notices).build(k).key("about-notices"));
    let mut c = col().key("about").w_full().gap(6.0).px(24.0).pt(35.0).pb(24.0).items_center()
        .child(tile.mb(1.0))
        .child(text("Filewinch").key("about-name").font(k.ty.ui_medium).color(k.c.text_strong).role("heading").level(1))
        .child(text(format!("Version {}", env!("CARGO_PKG_VERSION"))).key("about-version").font(k.ty.text_small).color(k.c.text_muted).selectable(true))
        .child(text(crate::license_ui::status_line(&s.license)).key("about-status").font(k.ty.text_small_medium).color(status_color).mt(8.0))
        .child(text(COPYRIGHT).key("about-copyright").font(k.ty.text_xsmall).color(k.c.text_muted).mt(10.0))
        .child(text(LICENSED).key("about-license").font(k.ty.text_xsmall).color(k.c.text_muted))
        .child(text(SOURCE_URL).key("about-url").font(k.ty.text_xsmall).color(paint.accent).selectable(true))
        .child(buttons.mt(14.0));
    if let Some(e) = &s.error {
        c = c.child(text(e.clone()).font(k.ty.text_xsmall).color(paint.danger).wrap().role("alert"));
    }
    c
}

fn notices_page(s: &AboutUi, k: &Kit) -> El {
    let n = notices::get();
    let back = IconButton::new(&icons::BACK).variant(IconButtonVariant::Ghost).control_size(ControlSize::Small).label("Back to About")
        .on_press(|s: &mut AboutUi| s.page = Page::About).build(k).key("notices-back");
    let head = row().w_full().gap(6.0).center_y().child(back)
        .child(text("Third-Party Notices").font(k.ty.text_small_medium).color(k.c.text_strong).role("heading").level(1));
    let mut list = col().key("notices-list").role("list").label("Licences").w_full().gap(4.0);
    for (i, g) in n.groups.iter().enumerate() {
        let open = s.open == Some(i);
        let title = format!("{} · {}", g.title, if g.used_by.len() == 1 { "1 component".to_owned() } else { format!("{} components", g.used_by.len()) });
        let header = row().key(format!("lic-{i}")).role("button").label(title.clone()).expanded(open).w_full().min_h(28.0).px(8.0).gap(6.0).center_y().radius(6.0)
            .hover(k.c.hover)
            .on_press(move |s: &mut AboutUi| s.open = if s.open == Some(i) { None } else { Some(i) })
            .child(icon(if open { &icons::UP } else { &icons::FORWARD }, 12.0).color(k.c.text_muted).a11y_hidden())
            .child(text(title).font(k.ty.text_small).color(k.c.text_base).grow(1.0).min_w(0.0).truncate());
        let mut item = col().role("listitem").w_full().child(header);
        if open {
            item = item.child(col().w_full().gap(8.0).px(26.0).pb(8.0)
                .child(text(format!("Used by: {}.", g.used_by.join(", "))).font(k.ty.text_xsmall).color(k.c.text_muted).wrap().selectable(true))
                .child(text(g.text.clone()).key(format!("lic-text-{i}")).font(k.ty.mono).color(k.c.text_base).wrap().selectable(true)));
        }
        list = list.child(item);
    }
    col().key("notices").w_full().gap(10.0).px(16.0).pt(36.0).pb(16.0)
        .child(head)
        .child(text(format!("Filewinch is built on {} open-source components. Choose a licence to read it and see who uses it.", n.components))
            .font(k.ty.text_xsmall).color(k.c.text_muted).wrap())
        .child(list)
}

pub fn view(s: &AboutUi, cx: &mut Cx<AboutUi>) -> El {
    let dark = theme::is_dark(s.appearance, cx.system_dark());
    theme::follow(cx, dark);
    let k = &*theme::kit(dark);
    let body = match s.page {
        Page::About => about_page(s, k, cx.env().scale),
        Page::Notices => notices_page(s, k),
    };
    let page = col().key("about-page").w_full().h_full().fill(k.c.bg_base)
        .child(rune::el().key("about-drag").w_full().h(28.0).window_drag().pin(Some(0.0), Some(0.0), Some(0.0), None))
        .child(col().w_full().grow(1.0).min_h(0.0).scroll(rune::ScrollSpec { stick_bottom: false }).child(body));
    rune_kit::root(page, Vec::new())
}

fn on_key(s: &mut AboutUi, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if ev.key == rune::Key::Escape && s.page == Page::Notices {
        s.page = Page::About;
        return true;
    }
    if ev.key == rune::Key::Command('w') || ev.key == rune::Key::Escape {
        tx.close_window();
        return true;
    }
    false
}

fn setup(_: &mut AboutUi, waker: Waker) {
    theme::watch(waker);
}

fn poll(_: &mut AboutUi, _: f64, _: &mut Tx) -> bool {
    static SEEN: AtomicU64 = AtomicU64::new(0);
    theme::accent_changed(&SEEN)
}

#[cfg(any(test, feature = "dev-tools"))]
type Scene = (&'static str, fn(&mut AboutUi));

#[cfg(any(test, feature = "dev-tools"))]
const SCENES: &[Scene] = &[
    ("dark", |s| s.appearance = Appearance::Dark),
    ("light", |s| s.appearance = Appearance::Light),
    ("unregistered", |s| s.license = Status::Unregistered),
    ("notices", |s| {
        s.page = Page::Notices;
        s.open = Some(0);
    }),
];

pub fn spec() -> AppSpec<AboutUi> {
    let mut spec = AppSpec {
        on_key: Some(on_key),
        setup: Some(setup),
        poll: Some(poll),
        min_size: Some((420.0, 420.0)),
        #[cfg(any(test, feature = "dev-tools"))]
        scenes: SCENES,
        ..AppSpec::new("About Filewinch", (460.0, 480.0), AboutUi::new, view)
    };
    if let Some(families) = theme::families() {
        spec.families = families;
    }
    spec
}
