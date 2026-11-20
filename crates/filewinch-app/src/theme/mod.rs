//! Filewinch's look follows the platform it runs on: macOS (Rune's native kit and the system
//! accent), Windows 11 (Fluent tokens and the Windows accent) and Linux (libadwaita, or Breeze
//! colours under KDE, and the portal's accent). The look is picked by `target_os`; the hidden
//! `--look macos|windows|linux|kde` flag overrides it so headless snapshots can render every
//! look on one machine. The look also decides the OS the app's wording and shortcut labels
//! follow ([`ui_os`]); behaviour (paths, secret store, launching, key handling) stays on
//! `Os::CURRENT`.
//!
//! Kits are cached by (look, appearance, accent), so an accent change builds a new one and the
//! views that read it rebuild (`version` keys row caches).

mod looks;
#[cfg(test)]
mod tests;
pub mod tone;

use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};

use filewinch_os::Os;
use filewinch_store::Appearance;
use rune::{Color, Cx, KeyPlatform, SystemAccent, TextStyle, Waker};
use rune_kit::Kit;

use tone::Tone;

/// A platform's look.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Look {
    MacOs,
    /// Windows 11 Fluent.
    Windows,
    /// GNOME libadwaita, the Linux default.
    Gnome,
    /// libadwaita's structure with KDE Breeze colours and fonts.
    Kde,
}

impl Look {
    /// The look for `os` (`std::env::consts::OS`) and `XDG_CURRENT_DESKTOP`.
    pub fn native(os: &str, desktop: Option<&str>) -> Look {
        match os {
            "macos" => Look::MacOs,
            "windows" => Look::Windows,
            _ if desktop.is_some_and(filewinch_platform_look::is_kde) => Look::Kde,
            _ => Look::Gnome,
        }
    }

    /// `--look`'s value.
    pub fn parse(s: &str) -> Option<Look> {
        match s.to_ascii_lowercase().as_str() {
            "macos" | "mac" => Some(Look::MacOs),
            "windows" | "fluent" => Some(Look::Windows),
            "linux" | "gnome" | "adwaita" => Some(Look::Gnome),
            "kde" | "breeze" => Some(Look::Kde),
            _ => None,
        }
    }

    /// The OS whose wording and shortcut labels this look shows ("This Computer", "Ctrl+K").
    pub fn os(self) -> Os {
        match self {
            Look::MacOs => Os::Mac,
            Look::Windows => Os::Windows,
            Look::Gnome | Look::Kde => Os::Linux,
        }
    }

    /// The default accent when the platform reports none (and on macOS, headless).
    fn default_accent(self, dark: bool) -> Color {
        match self {
            Look::MacOs => SystemAccent::DEFAULT.accent(dark),
            Look::Windows => tone::rgb(filewinch_platform_look::FLUENT_DEFAULT.base),
            Look::Gnome => tone::rgb([0x35, 0x84, 0xe4]),
            Look::Kde => tone::rgb([0x3d, 0xae, 0xe9]),
        }
    }

    /// The sans and mono families (`AppSpec::families`), or `None` to keep Rune's default.
    pub fn families(self) -> Option<(&'static str, &'static str)> {
        match self {
            Look::MacOs => None,
            Look::Windows => Some(("Segoe UI Variable Text, Segoe UI, sans-serif", "ui-monospace")),
            Look::Gnome => Some(("Adwaita Sans, Cantarell, sans-serif", "ui-monospace")),
            Look::Kde => Some(("Noto Sans, sans-serif", "ui-monospace")),
        }
    }

    /// A file row's height: Finder's 24, Fluent's 32 (ListView's compact item height), libadwaita's
    /// 34 and Breeze's 30.
    pub fn row_height(self) -> f32 {
        match self {
            Look::MacOs => 24.0,
            Look::Windows => 32.0,
            Look::Gnome => 34.0,
            Look::Kde => 30.0,
        }
    }
}

static LOOK: OnceLock<Look> = OnceLock::new();

/// The active look: the `--look` override, else this platform's.
pub fn look() -> Look {
    *LOOK.get_or_init(|| Look::native(std::env::consts::OS, std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref()))
}

/// The OS the app *words* things for: menu names, "This Mac" / "This Computer", the file manager
/// and secret store it names, window titles and shortcut labels. It is `Os::CURRENT` unless
/// `--look` (read once, before the first window) asked for another platform's look, so headless
/// snapshots of the Windows and Linux looks say what those platforms say. It never changes
/// behaviour: paths, the secret store, launching and key handling use `Os::CURRENT`.
pub fn ui_os() -> Os {
    #[cfg(test)]
    if let Some(os) = TEST_UI_OS.with(std::cell::Cell::get) {
        return os;
    }
    look().os()
}

/// How [`ui_os`] writes shortcuts: "⌘K" on macOS, "Ctrl+K" elsewhere.
pub fn key_platform() -> KeyPlatform {
    match ui_os() {
        Os::Mac => KeyPlatform::Mac,
        Os::Windows => KeyPlatform::Windows,
        Os::Linux => KeyPlatform::Linux,
    }
}

#[cfg(test)]
thread_local! {
    static TEST_UI_OS: std::cell::Cell<Option<Os>> = const { std::cell::Cell::new(None) };
}

/// Tests: word this thread's views for `os` (`None` goes back to the build's own), as `--look`
/// does for a whole run. Each test runs on its own thread, so tests can't see each other's.
#[cfg(test)]
pub fn set_test_ui_os(os: Option<Os>) {
    TEST_UI_OS.with(|c| c.set(os));
}

/// Applies `--look NAME` from the command line (a debug flag for headless snapshots). Call before
/// the first window; an unknown name is reported and ignored. Also takes `--appearance`.
pub fn init_from_args(args: impl IntoIterator<Item = String>) {
    let args: Vec<String> = args.into_iter().collect();
    if let Some(dark) = appearance_arg(args.iter().cloned()) {
        let _ = FORCE_DARK.set(dark);
    }
    match look_arg(args) {
        Some(Ok(l)) => {
            let _ = LOOK.set(l);
        }
        Some(Err(v)) => eprintln!("filewinch: --look takes macos, windows, linux or kde, not {v:?}"),
        None => {}
    }
}

static FORCE_DARK: OnceLock<bool> = OnceLock::new();

/// `--appearance light|dark`: a debug flag that draws every window in that appearance whatever
/// the scene or preference says, so snapshots can show each look both ways.
fn appearance_arg(args: impl IntoIterator<Item = String>) -> Option<bool> {
    let mut args = args.into_iter();
    while let Some(a) = args.next() {
        if a == "--appearance" {
            return match args.next().as_deref() {
                Some("dark") => Some(true),
                Some("light") => Some(false),
                _ => None,
            };
        }
    }
    None
}

/// `--look NAME` or `--look=NAME` among `args`: the look, or the name it didn't know.
fn look_arg(args: impl IntoIterator<Item = String>) -> Option<Result<Look, String>> {
    let mut args = args.into_iter();
    while let Some(a) = args.next() {
        let value = match a.strip_prefix("--look=") {
            Some(v) => v.to_owned(),
            None if a == "--look" => args.next().unwrap_or_default(),
            None => continue,
        };
        return Some(Look::parse(&value).ok_or(value));
    }
    None
}

/// The families the active look draws text in (`AppSpec::families`), if not Rune's default.
pub fn families() -> Option<(&'static str, &'static str)> {
    look().families()
}

/// The macOS accent and focus ring per appearance, as the last view read them from Rune.
static MAC_ACCENT: Mutex<[Option<(Color, Color)>; 2]> = Mutex::new([None, None]);

/// Reads the macOS accent through `cx` (so the view re-runs when the user picks another) and
/// keeps it for `kit`. Main and Settings views call it before building. Elsewhere it does nothing:
/// the platform readers ([`start_watching`]) keep the accent there.
pub fn follow<S>(cx: &Cx<S>, dark: bool) {
    if look() != Look::MacOs {
        return;
    }
    let (accent, focus) = (cx.accent(), cx.focus_color());
    let pair = if dark == cx.system_dark() {
        (accent, focus)
    } else if crate::hub::sandboxed() {
        // Headless and tests: the fixed default, so snapshots don't depend on the machine.
        let d = SystemAccent::DEFAULT;
        (d.accent(dark), d.focus(dark))
    } else {
        // The app's appearance setting differs from the system's: AppKit's colours for it.
        let a = rune::system_accent();
        (a.accent(dark), a.focus(dark))
    };
    MAC_ACCENT.lock().unwrap_or_else(|e| e.into_inner())[dark as usize] = Some(pair);
}

/// Settings-window wakers, woken when the platform accent changes.
static WAKERS: Mutex<Vec<Waker>> = Mutex::new(Vec::new());

/// Registers a window's waker (its `poll` should return [`accent_changed`]).
pub fn watch(waker: Waker) {
    WAKERS.lock().unwrap_or_else(|e| e.into_inner()).push(waker);
}

/// Starts the Windows or Linux accent reader, off the UI thread (from `on_ready`, so the first
/// frame never waits for it). Each change rebuilds every window. Nothing to start on macOS.
pub fn start_watching() {
    filewinch_platform_look::start(|| {
        crate::hub::hub().each(|inbox| inbox.post(|_| {}));
        for w in WAKERS.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            w();
        }
    });
}

/// True once per platform accent change, for a `poll` without other state: `seen` holds the
/// version last reported.
pub fn accent_changed(seen: &std::sync::atomic::AtomicU64) -> bool {
    let v = filewinch_platform_look::version();
    seen.swap(v, std::sync::atomic::Ordering::Relaxed) != v
}

/// The active look's accent for an appearance.
fn tone(look: Look, dark: bool) -> Tone {
    match look {
        Look::MacOs => {
            let stored = MAC_ACCENT.lock().unwrap_or_else(|e| e.into_inner())[dark as usize];
            let (accent, focus) = stored.unwrap_or((look.default_accent(dark), SystemAccent::DEFAULT.focus(dark)));
            tone::mac(accent, focus)
        }
        Look::Windows => {
            let a = filewinch_platform_look::current().unwrap_or(filewinch_platform_look::FLUENT_DEFAULT);
            let (bg, fg) = if dark { ("#2b2b2b", "#ffffff") } else { ("#fbfbfb", "#1b1b1b") };
            tone::fluent(a, dark, hex(bg), hex(fg))
        }
        Look::Gnome | Look::Kde => {
            let base = filewinch_platform_look::current().map_or(look.default_accent(dark), |a| tone::rgb(a.base));
            let bg = match (look, dark) {
                (Look::Gnome, true) => "#1d1d20",
                (Look::Kde, true) => "#141618",
                _ => "#ffffff",
            };
            tone::adwaita(base, hex(bg))
        }
    }
}

fn hex(s: &str) -> Color {
    rune::color::hex(s).unwrap_or(Color::BLACK)
}

/// Builds `look`'s kit for an appearance and accent.
pub fn build(look: Look, dark: bool, t: Tone) -> Kit {
    match look {
        Look::MacOs => looks::mac(dark, t),
        Look::Windows => looks::fluent(dark, t),
        Look::Gnome => looks::adwaita(dark, t),
        Look::Kde => looks::breeze(dark, t),
    }
}

type Key = (Look, bool, [u32; 4], [u32; 4], [u32; 4], [u32; 4]);

fn key(look: Look, dark: bool, t: &Tone) -> Key {
    let bits = |c: Color| [c.r, c.g, c.b, c.a].map(f32::to_bits);
    (look, dark, bits(t.fill), bits(t.on_fill), bits(t.text), bits(t.focus))
}

/// How many kits stay cached: two appearances of the current accent, plus a few recent ones.
const CACHE: usize = 6;
static KITS: Mutex<Vec<(Key, Arc<Kit>)>> = Mutex::new(Vec::new());

/// The kit for the active look, an appearance and the current accent.
pub fn kit(dark: bool) -> Arc<Kit> {
    cached(look(), dark, tone(look(), dark))
}

fn cached(look: Look, dark: bool, t: Tone) -> Arc<Kit> {
    let key = key(look, dark, &t);
    let mut kits = KITS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(i) = kits.iter().position(|(k, _)| *k == key) {
        let hit = kits.remove(i);
        kits.push(hit.clone());
        return hit.1;
    }
    let kit = Arc::new(build(look, dark, t));
    if kits.len() >= CACHE {
        kits.remove(0);
    }
    kits.push((key, kit.clone()));
    kit
}

/// A hash of what the kit for `dark` depends on, for caches of built elements
/// (`virtual_rows_version`): it changes with the appearance and the accent.
pub fn version(dark: bool) -> u64 {
    // `DefaultHasher::new` has fixed keys, so equal keys hash equal across calls.
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key(look(), dark, &tone(look(), dark)).hash(&mut h);
    h.finish()
}

/// Whether to draw dark, given the preference and the system's appearance.
pub fn is_dark(pref: Appearance, system_dark: bool) -> bool {
    if let Some(dark) = FORCE_DARK.get() {
        return *dark;
    }
    match pref {
        Appearance::System => system_dark,
        Appearance::Light => false,
        Appearance::Dark => true,
    }
}

/// `k` restyled for the window's default-action button: the accent fill with its text colour
/// (macOS's default button, Fluent's `AccentButton`, Adwaita's `suggested-action`). Build a
/// `ButtonVariant::Contrast` button with it, which keeps rune-kit's sizing, focus ring and
/// accessibility: `Button::new("Connect").variant(Contrast).build(&theme::primary(k))`.
pub fn primary(k: &Kit) -> Kit {
    let mut p = k.clone();
    let c = &mut p.c;
    c.bg_contrast = c.bg_accent;
    c.text_contrast = if c.knob_on.luminance() < 0.5 { c.knob_on } else { Color::WHITE };
    c.contrast_hover = Color::WHITE.alpha(0.1);
    c.contrast_pressed = Color::BLACK.alpha(0.12);
    if look() != Look::MacOs {
        // Fluent and Adwaita accent buttons are flat: the contrast skin's top sheen is mixed from
        // `c.white`, so make it the fill (only in this button's kit).
        c.white = c.bg_accent;
    }
    p
}

/// Colours and metrics the app paints beyond the kit's tokens.
#[derive(Clone, Copy)]
pub struct Paint {
    pub dark: bool,
    pub accent: Color,
    /// The window behind panes.
    pub chrome: Color,
    /// A pane's list.
    pub list: Color,
    pub hair: Color,
    /// A selected row in the focused pane, and in the other one.
    pub selected: Color,
    pub selected_inactive: Color,
    /// Text and icons on a focused selection: Finder's white on the accent. `None` keeps them.
    pub selected_ink: Option<Color>,
    /// Fluent's selection indicator, a short accent bar at a selected row's leading edge.
    pub selection_pill: Option<Color>,
    /// A selected sidebar row.
    pub sidebar_selected: Color,
    pub stripe: Color,
    pub danger: Color,
    pub success: Color,
    pub warning: Color,
    /// Folder icons: the accent on macOS, Explorer's yellow, Adwaita's folder blue.
    pub folder: Color,
    pub row_h: f32,
    /// The window title at the leading edge (Windows) rather than centred.
    pub title_leading: bool,
}

pub fn paint(k: &Kit) -> Paint {
    paint_for(look(), k)
}

/// [`paint`] for `look`'s kit `k`.
fn paint_for(look: Look, k: &Kit) -> Paint {
    let c = &k.c;
    let ink = |a: f32| k.alpha(c.text_base, a);
    let tint = |a: f32| k.alpha(c.bg_accent, a);
    let (selected, selected_inactive, selected_ink, selection_pill, sidebar_selected, stripe, folder) = match look {
        // Finder: the focused list's selection is a solid accent (AppKit's
        // `selectedContentBackgroundColor`, a step darker than the accent) with white text; an
        // unfocused one is grey. Alternating rows.
        Look::MacOs => (
            tone::lighten(c.bg_accent, if k.dark { -0.13 } else { -0.07 }),
            if k.dark { rune::color::hex("#464646").unwrap_or(c.layer_03) } else { rune::color::hex("#dcdcdc").unwrap_or(c.layer_03) },
            Some(Color::WHITE),
            None,
            ink(if k.dark { 0.12 } else { 0.10 }),
            ink(if k.dark { 0.025 } else { 0.03 }),
            c.text_accent,
        ),
        // Fluent: an accent-tinted subtle fill with the accent pill; no stripes.
        Look::Windows => (
            tint(if k.dark { 0.20 } else { 0.12 }),
            ink(if k.dark { 0.11 } else { 0.09 }),
            None,
            Some(c.bg_accent),
            ink(if k.dark { 0.08 } else { 0.06 }),
            Color::TRANSPARENT,
            rune::oklch(0.82, 0.15, 82.0),
        ),
        // libadwaita: `alpha(accent_bg_color, .25)` in lists, `alpha(currentColor, .1)` in
        // sidebars; no stripes.
        Look::Gnome | Look::Kde => (
            tint(if k.dark { 0.30 } else { 0.22 }),
            ink(if k.dark { 0.12 } else { 0.09 }),
            None,
            None,
            ink(0.10),
            Color::TRANSPARENT,
            if look == Look::Gnome { rune::color::hex("#438de6").unwrap_or(c.text_accent) } else { c.text_accent },
        ),
    };
    Paint {
        dark: k.dark,
        accent: c.text_accent,
        chrome: c.bg_deep,
        list: c.bg_base,
        hair: c.border_weak,
        selected,
        selected_inactive,
        selected_ink,
        selection_pill,
        sidebar_selected,
        stripe,
        danger: c.danger.fg,
        success: c.success.fg,
        warning: c.warning.fg,
        folder,
        row_h: look.row_height(),
        title_leading: look == Look::Windows,
    }
}

/// Fluent's selection pill for a row `h` tall, drawn at its leading edge, if the look has one.
pub fn pill(paint: &Paint, h: f32) -> Option<rune::El> {
    let color = paint.selection_pill?;
    let ph = (h * 0.5).min(16.0);
    Some(rune::el().absolute(0.0, (h - ph) / 2.0).w(3.0).h(ph).radius(1.5).fill(color).a11y_hidden())
}

/// Small caps-like section headings in the sidebar and sheets.
pub fn section(k: &Kit) -> TextStyle {
    k.ty.text_xsmall_medium
}
