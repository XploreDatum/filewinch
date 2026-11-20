//! The Settings window: transfers (concurrency, retries, bandwidth, conflicts), browsing
//! (hidden files, permissions, delete confirmation), appearance and the external editor.
//! Each change saves at once and reaches every open window.

use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;

use filewinch_store::{Appearance, ConflictDefault, Prefs};
use rune::{AppHandle, AppSpec, Cx, El, KeyEvent, PopupMenu, TextEdit, Tx, Waker, WindowHandle, col, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, Kit, SegmentedControl, Switch, TextInput};

use crate::hub::hub;
use crate::icons;
use crate::theme;

pub struct SettingsUi {
    pub prefs: Prefs,
    pub bandwidth: TextEdit,
    pub bandwidth_error: Option<String>,
    pub editor: TextEdit,
    pub saved_error: Option<String>,
    pub system_dark: bool,
    /// The paid licence (see `license_ui::settings`).
    pub license: filewinch_license::Status,
    pub license_busy: bool,
    pub license_error: Option<String>,
    /// "Deactivate This Device…" was pressed: ask once, inline.
    pub confirm_deactivate: bool,
}

impl SettingsUi {
    pub fn new(prefs: Prefs) -> SettingsUi {
        SettingsUi {
            bandwidth: TextEdit::new(prefs.bandwidth_limit.map(|b| (b / 1000).to_string()).unwrap_or_default()),
            editor: TextEdit::new(prefs.editor.clone().unwrap_or_default()),
            prefs,
            bandwidth_error: None,
            saved_error: None,
            system_dark: false,
            license: crate::license_ui::initial(),
            license_busy: false,
            license_error: None,
            confirm_deactivate: false,
        }
    }

    /// Saves and hands the new prefs to the engine and every main window.
    fn commit(&mut self) {
        self.prefs = self.prefs.clone().clamped();
        hub().engine.set_config(crate::ui::engine_config(&self.prefs));
        self.saved_error = hub().store.as_ref().and_then(|s| s.save_prefs(&self.prefs).err()).map(|e| e.message);
        let prefs = self.prefs.clone();
        hub().each(|inbox| {
            let p = prefs.clone();
            inbox.post(move |ui| ui.apply_prefs(p));
        });
    }
}

static WINDOW: OnceLock<(AppHandle<SettingsUi>, WindowHandle)> = OnceLock::new();

/// Registers the Settings window built at startup.
pub fn install(handle: AppHandle<SettingsUi>, window: WindowHandle) {
    let _ = WINDOW.set((handle, window));
}

/// Shows the Settings window with `prefs`.
pub fn open_settings(prefs: &Prefs) {
    let Some((h, w)) = WINDOW.get() else { return };
    let prefs = prefs.clone();
    // `new` reads the licence status afresh.
    h.update(move |s, _| *s = SettingsUi { system_dark: s.system_dark, ..SettingsUi::new(prefs) });
    w.open();
    w.focus();
}

/// Keeps an open Settings window in step with a change made elsewhere (a menu toggle).
pub fn push_prefs(prefs: &Prefs) -> Option<()> {
    let (h, _) = WINDOW.get()?;
    let prefs = prefs.clone();
    h.update(move |s, _| s.prefs = prefs);
    Some(())
}

/// Keeps an open Settings window's licence section current.
pub fn push_license(status: filewinch_license::Status) {
    if let Some((h, _)) = WINDOW.get() {
        h.update(move |s, _| {
            s.license = status;
            s.license_busy = false;
            s.confirm_deactivate = false;
        });
    }
}

/// Runs `f` on the Settings window's state from any thread.
pub fn update(f: impl FnOnce(&mut SettingsUi) + Send + 'static) {
    if let Some((h, _)) = WINDOW.get() {
        h.update(move |s, _| f(s));
    }
}

fn stepper(k: &Kit, key: &'static str, value: u32, min: u32, max: u32, set: fn(&mut Prefs, u32)) -> El {
    let minus = IconButton::new(&icons::MINUS).control_size(ControlSize::Small).label(format!("Fewer ({key})")).disabled(value <= min)
        .on_press(move |s: &mut SettingsUi| {
            set(&mut s.prefs, value.saturating_sub(1).max(min));
            s.commit();
        }).build(k).key("minus");
    let plus = IconButton::new(&icons::PLUS).control_size(ControlSize::Small).label(format!("More ({key})")).disabled(value >= max)
        .on_press(move |s: &mut SettingsUi| {
            set(&mut s.prefs, (value + 1).min(max));
            s.commit();
        }).build(k).key("plus");
    row().key(key).role("spinbutton").label(key).a11y_value(value.to_string()).range_value(value as f64, min as f64, max as f64)
        .on_step(move |s: &mut SettingsUi, steps, _tx: &mut Tx| {
            set(&mut s.prefs, (value as i64 + steps as i64).clamp(min as i64, max as i64) as u32);
            s.commit();
        })
        .gap(6.0).center_y()
        .child(minus)
        .child(text(value.to_string()).font(k.ty.mono).color(k.c.text_base).w(28.0).align(rune::Align::Center).a11y_hidden())
        .child(plus)
}

/// The warning under "Connections per download" above the safe default.
pub const CONNECTIONS_WARNING: &str = "More connections can be unstable, and some servers limit or refuse parallel downloads.";

/// "Connections per download": 1–16 (1 is off), with a caution above 3.
fn connections(k: &Kit, value: u32) -> El {
    let control = stepper(k, "Connections per download", value, 1, Prefs::MAX_SEGMENTS, |p, v| p.segments_per_file = v);
    let row = setting(k, "Connections per download", Some("Splits a large download into parts fetched at once. 1 turns it off."), control);
    if value <= Prefs::SAFE_SEGMENTS {
        return row;
    }
    let warning = text(CONNECTIONS_WARNING).font(k.ty.text_xsmall).color(k.c.warning.fg).wrap().grow(1.0).min_w(0.0).live_region(rune::LiveRegion::Polite);
    col().key("connections").w_full().child(row).child(row_warning(warning))
}

fn row_warning(t: El) -> El {
    row().key("connections-warning").w_full().px(14.0).pb(8.0).mt(-4.0).child(t)
}

pub fn setting(k: &Kit, label: &str, detail: Option<&str>, control: El) -> El {
    let mut words = col().gap(2.0).grow(1.0).min_w(0.0).child(text(label.to_owned()).font(k.ty.text_small).color(k.c.text_base));
    if let Some(d) = detail {
        words = words.child(text(d.to_owned()).font(k.ty.text_xsmall).color(k.c.text_muted).wrap());
    }
    row().w_full().min_h(40.0).gap(16.0).px(14.0).py(6.0).center_y().child(words).child(control)
}

pub fn group(k: &Kit, key: &str, title: &str, rows: Vec<El>) -> El {
    let mut card = col().key(format!("{key}-card")).w_full().fill(k.c.surface_raised_base).radius(8.0).border(1.0, k.c.border_weak);
    let n = rows.len();
    for (i, r) in rows.into_iter().enumerate() {
        card = card.child(if i + 1 < n { r.border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak) } else { r });
    }
    col().key(key.to_owned()).gap(6.0).w_full()
        .child(text(title.to_owned()).font(theme::section(k)).color(k.c.text_muted).ml(4.0).role("heading").level(2))
        .child(card)
}

fn switch(k: &Kit, key: &str, label: &str, on: bool, flip: fn(&mut Prefs)) -> El {
    Switch::new(on).label(label).hide_label().on_toggle(move |s: &mut SettingsUi| {
        flip(&mut s.prefs);
        s.commit();
    }).build(k).key(key.to_owned())
}

pub fn view(s: &SettingsUi, cx: &mut Cx<SettingsUi>) -> El {
    let dark = theme::is_dark(s.prefs.appearance, cx.system_dark());
    theme::follow(cx, dark);
    let k = &*theme::kit(dark);
    let p = &s.prefs;
    let conflict = Button::new(p.conflict.label()).variant(ButtonVariant::Neutral).control_size(ControlSize::Small).expanded(false)
        .on_press_tx(|s: &mut SettingsUi, tx: &mut Tx| {
            let options: Vec<(&str, &str)> = ConflictDefault::ALL.iter().map(|c| (conflict_id(*c), c.label())).collect();
            let menu = PopupMenu::picker(options, conflict_id(s.prefs.conflict));
            tx.popup_menu(menu, "conflict", |s: &mut SettingsUi, picked, _tx: &mut Tx| {
                if let Some(c) = picked.and_then(|id| ConflictDefault::ALL.into_iter().find(|c| conflict_id(*c) == id)) {
                    s.prefs.conflict = c;
                    s.commit();
                }
            });
        }).build(k).key("conflict");
    let bandwidth = TextInput::new(&s.bandwidth).label("Bandwidth limit in kilobytes per second").placeholder("Unlimited").width(Some(110.0))
        .invalid(s.bandwidth_error.is_some())
        .on_edit(|s: &mut SettingsUi| &mut s.bandwidth)
        .on_change(|s: &mut SettingsUi, _tx: &mut Tx| {
            let t = s.bandwidth.text.trim();
            if t.is_empty() {
                s.bandwidth_error = None;
                s.prefs.bandwidth_limit = None;
                s.commit();
            } else {
                match t.parse::<u64>() {
                    Ok(kb) if kb > 0 => {
                        s.bandwidth_error = None;
                        s.prefs.bandwidth_limit = Some(kb.saturating_mul(1000));
                        s.commit();
                    }
                    _ => s.bandwidth_error = Some("Enter a whole number of KB/s, or leave it empty.".into()),
                }
            }
        })
        .build(k).key("bandwidth");
    let mut bw = col().gap(4.0).items_end().child(bandwidth);
    if let Some(e) = &s.bandwidth_error {
        bw = bw.child(text(e.clone()).font(k.ty.text_xsmall).color(k.c.danger.fg).live_region(rune::LiveRegion::Polite));
    }
    let transfers = group(k, "transfers", "Transfers", vec![
        setting(k, "Simultaneous transfers", Some("Jobs that run at the same time."), stepper(k, "Simultaneous transfers", p.max_jobs, 1, 16, |p, v| p.max_jobs = v)),
        setting(k, "Files per transfer", Some("Files one job moves in parallel; change it for a running job in Transfers. Each file can use several connections (below), so a job may open files × connections."), stepper(k, "Files per transfer", p.files_per_job, 1, 32, |p, v| p.files_per_job = v)),
        connections(k, p.segments_per_file),
        setting(k, "Retries", Some("After a dropped connection, with growing waits."), stepper(k, "Retries", p.retries, 0, 20, |p, v| p.retries = v)),
        setting(k, "Bandwidth limit", Some("KB/s across all transfers."), bw),
        setting(k, "When an item already exists", None, conflict),
    ]);
    let browsing = group(k, "browsing", "Browsing", vec![
        setting(k, "Show hidden files", Some(if cfg!(windows) { "Names starting with a dot, and items Windows marks hidden." } else { "Names starting with a dot." }), switch(k, "show-hidden", "Show hidden files", p.show_hidden, |p| p.show_hidden = !p.show_hidden)),
        setting(k, "Show permissions column", None, switch(k, "show-perms", "Show permissions column", p.show_permissions, |p| p.show_permissions = !p.show_permissions)),
        setting(k, "Ask before deleting", None, switch(k, "confirm-delete", "Ask before deleting", p.confirm_deletes, |p| p.confirm_deletes = !p.confirm_deletes)),
    ]);
    let looks = ["System", "Light", "Dark"];
    let current = match p.appearance {
        Appearance::System => 0,
        Appearance::Light => 1,
        Appearance::Dark => 2,
    };
    let appearance = SegmentedControl::new(&looks, Some(current)).label("Appearance").fit()
        .on_pick(|s: &mut SettingsUi, i, tx: &mut Tx| {
            s.prefs.appearance = [Appearance::System, Appearance::Light, Appearance::Dark][i.min(2)];
            tx.color_motion(rune::tok::motion::CALM);
            s.commit();
        }).build(k).key("appearance");
    let editor = TextInput::new(&s.editor).label("External editor").placeholder("Default app for each file").width(Some(200.0))
        .on_edit(|s: &mut SettingsUi| &mut s.editor)
        .on_change(|s: &mut SettingsUi, _tx: &mut Tx| {
            s.prefs.editor = Some(s.editor.text.trim().to_owned()).filter(|e| !e.is_empty());
            s.commit();
        })
        .build(k).key("editor");
    let general = group(k, "general", "General", vec![
        setting(k, "Appearance", None, appearance),
        setting(k, "External editor", Some(editor_hint(crate::theme::ui_os())), editor),
    ]);
    let mut body = col().key("settings").w_full().gap(18.0).px(22.0).pt(36.0).pb(22.0)
        .child(text(crate::keys::Act::Settings.title().trim_end_matches('…')).font(k.ty.ui_medium).color(k.c.text_strong).role("heading").level(1))
        .child(general).child(crate::license_ui::settings::group_view(s, k)).child(transfers).child(browsing);
    if let Some(e) = &s.saved_error {
        body = body.child(text(e.clone()).font(k.ty.text_small).color(k.c.danger.fg).role("alert"));
    }
    let page = col().key("settings-page").w_full().h_full().fill(k.c.bg_base)
        .child(rune::el().key("settings-drag").w_full().h(28.0).window_drag().pin(Some(0.0), Some(0.0), Some(0.0), None))
        .child(col().w_full().grow(1.0).min_h(0.0).scroll(rune::ScrollSpec { stick_bottom: false }).child(body));
    rune_kit::root(page, Vec::new())
}

fn conflict_id(c: ConflictDefault) -> &'static str {
    match c {
        ConflictDefault::Ask => "ask",
        ConflictDefault::Overwrite => "overwrite",
        ConflictDefault::OverwriteIfNewer => "newer",
        ConflictDefault::Resume => "resume",
        ConflictDefault::Rename => "rename",
        ConflictDefault::Skip => "skip",
    }
}

fn on_key(_: &mut SettingsUi, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if ev.key == rune::Key::Command('w') || ev.key == rune::Key::Escape {
        tx.close_window();
        return true;
    }
    false
}

fn init() -> SettingsUi {
    SettingsUi::new(hub().store.as_ref().map(|s| s.load_prefs().value.clamped()).unwrap_or_default())
}

/// Rebuilds when the Windows or Linux accent changes (`theme::start_watching` wakes the window).
fn setup(_: &mut SettingsUi, waker: Waker) {
    theme::watch(waker);
}

fn poll(_: &mut SettingsUi, _: f64, _: &mut Tx) -> bool {
    // One Settings window per process, so one seen-version.
    static SEEN: AtomicU64 = AtomicU64::new(0);
    theme::accent_changed(&SEEN)
}

pub fn spec() -> AppSpec<SettingsUi> {
    let mut spec = AppSpec {
        on_key: Some(on_key),
        setup: Some(setup),
        poll: Some(poll),
        min_size: Some((520.0, 560.0)),
        #[cfg(any(test, feature = "dev-tools"))]
        scenes: &[
            ("dark", |s| s.prefs.appearance = Appearance::Dark),
            ("light", |s| s.prefs.appearance = Appearance::Light),
            ("license-unregistered", |s| s.license = filewinch_license::Status::Unregistered),
            ("license-deactivate", |s| s.confirm_deactivate = true),
            ("connections-warning", |s| s.prefs.segments_per_file = 8),
        ],
        ..AppSpec::new(window_title(crate::theme::ui_os()), (560.0, 760.0), init, view)
    };
    if let Some(families) = theme::families() {
        spec.families = families;
    }
    spec
}

/// The window's title: GNOME and KDE call it Preferences, macOS and Windows Settings.
pub fn window_title(os: filewinch_os::Os) -> &'static str {
    match os {
        filewinch_os::Os::Linux => "Filewinch Preferences",
        filewinch_os::Os::Mac | filewinch_os::Os::Windows => "Filewinch Settings",
    }
}

/// What the external-editor setting takes on each OS: an app name on macOS (`open -a`), a
/// command or program path elsewhere (the file is added at the end).
pub fn editor_hint(os: filewinch_os::Os) -> &'static str {
    match os {
        filewinch_os::Os::Mac => "An app name, like “BBEdit” or “Visual Studio Code”.",
        filewinch_os::Os::Windows => "A command or program path, like code or \"C:\\Program Files\\Notepad++\\notepad++.exe\". The file is added at the end.",
        filewinch_os::Os::Linux => "A command, like code, gedit or kate --new. The file is added at the end.",
    }
}
