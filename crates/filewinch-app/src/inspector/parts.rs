//! Pieces of the panel: a labelled fact, a link-style button, a foldable section, the preview
//! and the permissions editor.

use filewinch_core::EntryKind;
use rune::{Align, Color, El, ScrollSpec, SurfaceFit, Tx, col, el, icon, row, text};
use rune_kit::{Button, ButtonVariant, Checkbox, ControlSize, Kit, Spinner, TextInput};

use super::load::PREVIEW_MAX;
use super::perms::{self, Tri};
use super::{Inspector, Preview, Section};
use crate::fmt;
use crate::theme::Paint;
use crate::ui::Ui;

/// Width of a fact's label column.
const LABEL_W: f32 = 76.0;

/// "Label  value", the value wrapping and selectable; read as one line ("Modified: Today, 09:44").
/// `muted` greys the value (for "n/a").
pub fn fact(k: &Kit, key: &str, label: &str, value: impl Into<String>, muted: bool) -> El {
    let value = value.into();
    row().key(key.to_owned()).w_full().gap(8.0).items_start().label(format!("{label}: {value}"))
        .child(text(label.to_owned()).font(k.ty.text_xsmall).color(k.c.text_muted).w(LABEL_W).align(Align::End).pt(1.0).a11y_hidden())
        .child(text(value).font(k.ty.text_small).color(if muted { k.c.text_faint } else { k.c.text_base }).wrap_anywhere().grow(1.0).min_w(0.0).selectable(true).a11y_hidden())
}

/// A fact whose value is a date, "n/a" (muted) when the backend doesn't say.
pub fn date_fact(k: &Kit, key: &str, label: &str, t: Option<std::time::SystemTime>) -> El {
    match t {
        Some(t) => fact(k, key, label, fmt::date(t, fmt::now()), false),
        None => fact(k, key, label, "n/a", true),
    }
}

/// A fact with a trailing control (Copy, Calculate…), the control under the value's start.
pub fn fact_with(k: &Kit, key: &str, label: &str, value: El, control: El) -> El {
    row().key(key.to_owned()).w_full().gap(8.0).items_start()
        .child(text(label.to_owned()).font(k.ty.text_xsmall).color(k.c.text_muted).w(LABEL_W).align(Align::End).pt(1.0).a11y_hidden())
        .child(col().grow(1.0).min_w(0.0).gap(2.0).items_start().child(value).child(control))
}

/// A link-style button: accent text, no chrome until hovered; keyboard-reachable like any button.
pub fn link(k: &Kit, paint: &Paint, key: &str, label: &str, f: impl Fn(&mut Ui) + 'static) -> El {
    let (l, accent) = (label.to_owned(), paint.accent);
    Button::new(label).variant(ButtonVariant::Ghost).control_size(ControlSize::Small)
        .label_with(move |style, _| text(l).font(style).color(accent))
        .on_press(f).build(k).key(key.to_owned())
}

/// [`link`] whose action needs the transaction (to move focus).
pub fn link_tx(k: &Kit, paint: &Paint, key: &str, label: &str, f: impl Fn(&mut Ui, &mut Tx) + 'static) -> El {
    let (l, accent) = (label.to_owned(), paint.accent);
    Button::new(label).variant(ButtonVariant::Ghost).control_size(ControlSize::Small)
        .label_with(move |style, _| text(l).font(style).color(accent))
        .on_press_tx(f).build(k).key(key.to_owned())
}

/// A link that copies `value` to the clipboard.
pub fn copy_link(k: &Kit, paint: &Paint, key: &str, what: &str, value: String) -> El {
    let (l, accent) = (format!("Copy {what}"), paint.accent);
    Button::new(l.clone()).variant(ButtonVariant::Ghost).control_size(ControlSize::Small)
        .label_with(move |style, _| text(l).font(style).color(accent))
        .on_press_tx(move |_: &mut Ui, tx: &mut Tx| tx.copy(value.clone())).build(k).key(key.to_owned())
}

/// A foldable section: a header with a chevron (a button that says whether it's open), then
/// `body` unless folded. The open state is remembered in the preferences.
pub fn section(s: &Ui, k: &Kit, paint: &Paint, which: Section, body: impl FnOnce() -> El) -> El {
    let id = which.id();
    let open = !s.prefs.inspector_collapsed.iter().any(|c| c == id);
    let head = row().key(format!("section-{id}")).role("button").label(which.title()).expanded(open).focusable(true)
        .w_full().h(26.0).px(6.0).gap(6.0).center_y().radius(5.0).hover(k.c.hover)
        .on_press(move |s: &mut Ui| s.toggle_section(id))
        .on_key(move |s: &mut Ui, ev: &rune::KeyEvent, _tx: &mut Tx| {
            let toggle = matches!(ev.key, rune::Key::Enter) || matches!(&ev.key, rune::Key::Text(t) if t == " ");
            if toggle {
                s.toggle_section(id);
            }
            toggle
        })
        .child(icon(if open { k.glyph.chevron_down } else { k.glyph.caret_right }, 12.0).color(k.c.icon_muted).a11y_hidden())
        .child(text(which.title()).font(crate::theme::section(k)).color(k.c.text_muted).a11y_hidden());
    let mut c = col().key(format!("sec-{id}")).role("group").label(which.title()).w_full().gap(6.0).pt(6.0).border_sides([1.0, 0.0, 0.0, 0.0], paint.hair)
        .child(row().role("heading").level(3).label(which.title()).w_full().child(head));
    if open {
        c = c.child(col().w_full().gap(6.0).px(4.0).pb(10.0).child(body()));
    }
    c
}

/// An inline error with Retry.
pub fn failure(k: &Kit, paint: &Paint, key: &str, message: &str, retry: impl Fn(&mut Ui) + 'static) -> El {
    col().key(key.to_owned()).w_full().gap(2.0).items_start()
        .child(text(message.to_owned()).font(k.ty.text_small).color(paint.danger).wrap().live_region(rune::LiveRegion::Polite))
        .child(link(k, paint, &format!("{key}-retry"), "Retry", retry))
}

/// A muted line of explanation.
pub fn note(k: &Kit, s: impl Into<String>) -> El {
    text(s.into()).font(k.ty.text_xsmall).color(k.c.text_muted).wrap()
}

/// The preview section's body.
pub fn preview(i: &Inspector, name: &str, k: &Kit, paint: &Paint) -> El {
    let frame = |c: El| col().key("preview").w_full().radius(6.0).fill(paint.list).border(1.0, paint.hair).clip_soft(6.0).child(c);
    match &i.preview {
        Preview::None => el(),
        Preview::Loading { got, total } => {
            let got = got.load(std::sync::atomic::Ordering::Relaxed);
            let msg = match total {
                Some(t) if *t > 0 && got > 0 => format!("Loading preview… {} of {}", fmt::size(got), fmt::size(*t)),
                _ => "Loading preview…".into(),
            };
            row().key("preview-loading").gap(8.0).center_y().h(28.0)
                .child(Spinner::new().size(14.0).build(k).a11y_hidden())
                .child(text(msg).font(k.ty.text_small).color(k.c.text_muted).live_region(rune::LiveRegion::Polite))
        }
        Preview::TooLarge => col().key("preview-large").w_full().gap(4.0).items_start()
            .child(note(k, format!("Larger than {}, so it isn't downloaded automatically.", fmt::size(PREVIEW_MAX))))
            .child(link(k, paint, "preview-download", "Download to preview", |s: &mut Ui| s.inspector_preview(true))),
        Preview::Image(img) => match (img.size(), img.error()) {
            (_, Some(e)) => note(k, format!("Can't preview this image ({}).", e.0)),
            (Some([w, h]), _) if w > 0 && h > 0 => {
                let ratio = (w as f32 / h as f32).clamp(0.25, 4.0);
                frame(el().key("preview-image").role("img").label(format!("Preview of {name}, {w} × {h} pixels")).w_full().aspect_ratio(ratio).max_h(260.0)
                    .pixels_fit(img.pixels(), SurfaceFit::Fit))
                .child(text(format!("{w} × {h} pixels")).font(k.ty.text_xsmall).color(k.c.text_muted).px(8.0).py(4.0).a11y_hidden())
            }
            _ => row().key("preview-decoding").gap(8.0).center_y().h(28.0).child(Spinner::new().size(14.0).build(k).label("Decoding preview")),
        },
        Preview::Text { text: t, truncated } => {
            let mut c = col().w_full().gap(4.0).child(frame(
                col().key("preview-text").role("document").label(format!("Text of {name}")).w_full().max_h(240.0).min_h(0.0)
                    .scroll(ScrollSpec { stick_bottom: false })
                    .child(text(t.clone()).font(k.ty.mono).color(k.c.text_base).wrap().selectable(true).padding(8.0)),
            ));
            if *truncated {
                c = c.child(note(k, "Showing the first 64 KB."));
            }
            c
        }
        Preview::Binary => note(k, "No preview: this file isn't text."),
        Preview::Failed(e) => failure(k, paint, "preview-error", &format!("Couldn't load the preview. {}", e.message), |s: &mut Ui| s.inspector_preview(false)),
    }
}

const WHO: [(&str, u32); 3] = [("User", 6), ("Group", 3), ("World", 0)];
const WHAT: [(&str, &str, u32); 3] = [("Read", "read", 4), ("Write", "write", 2), ("Execute", "execute", 1)];
const SPECIAL: [(&str, &str, u32); 3] = [("SUID", "set user ID", 0o4000), ("SGID", "set group ID", 0o2000), ("Sticky", "sticky", 0o1000)];
const CELL: f32 = 46.0;
const WHO_W: f32 = 52.0;

fn bit_box(k: &Kit, modes: &[u32], i: &Inspector, bit: u32, label: String, key: String, enabled: bool) -> El {
    let t = perms::tri(modes, i.perms.staged, bit);
    let mut cb = Checkbox::new(t == Tri::On).indeterminate(t == Tri::Mixed).label(label).hide_label().disabled(!enabled);
    if t == Tri::Mixed {
        // Some items have it and some don't: a dash, not a tick.
        cb = cb.icon(k.glyph.dash);
    }
    let cb = cb.on_toggle(move |s: &mut Ui| s.toggle_perm_bit(bit)).build(k).key(key);
    row().w(CELL).justify_center().child(cb)
}

/// The permissions grid: read, write and execute for user, group and world, then the special
/// bits, with the octal field and the symbolic form kept in step.
pub fn perms_editor(i: &Inspector, modes: &[u32], any_dir: bool, k: &Kit, paint: &Paint) -> El {
    let p = &i.perms;
    let enabled = p.applying.is_none();
    let head = |labels: [&str; 3]| {
        let mut r = row().gap(0.0).child(el().w(WHO_W));
        for l in labels {
            r = r.child(text(l.to_owned()).font(k.ty.text_xsmall).color(k.c.text_muted).w(CELL).align(Align::Center).a11y_hidden());
        }
        r
    };
    let mut grid = col().key("perm-grid").role("group").label("Permissions").gap(2.0).child(head(["Read", "Write", "Execute"]));
    for (who, shift) in WHO {
        let mut r = row().gap(0.0).center_y().child(text(who).font(k.ty.text_small).color(k.c.text_base).w(WHO_W).a11y_hidden());
        for (_, what, bit) in WHAT {
            let b = bit << shift;
            r = r.child(bit_box(k, modes, i, b, format!("{who} can {what}"), format!("perm-{}-{what}", who.to_lowercase()), enabled));
        }
        grid = grid.child(r);
    }
    grid = grid.child(head(["SUID", "SGID", "Sticky"]).pt(4.0));
    let mut special = row().gap(0.0).center_y().child(text("Special").font(k.ty.text_small).color(k.c.text_base).w(WHO_W).a11y_hidden());
    for (short, long, bit) in SPECIAL {
        special = special.child(bit_box(k, modes, i, bit, format!("{short} ({long})"), format!("perm-{}", short.to_lowercase()), enabled));
    }
    grid = grid.child(special);
    let symbolic = perms::uniform(modes, p.staged).map_or_else(|| "mixed".to_owned(), fmt::mode_string);
    let octal = TextInput::new(&p.octal).label("Permissions in octal").placeholder(if modes.len() > 1 { "Mixed" } else { "0644" })
        .width(Some(72.0)).invalid(p.octal_error.is_some()).disabled(!enabled)
        .on_edit(|s: &mut Ui| &mut s.inspector.perms.octal)
        .on_change(|s: &mut Ui, _tx: &mut Tx| s.inspector.perms.typed())
        .on_submit(|s: &mut Ui, _tx: &mut Tx| {
            if s.inspector.dirty() && s.inspector.perms.octal_error.is_none() {
                s.request_apply(false);
            }
        })
        .text_style(k.ty.mono).build(k).key("perm-octal");
    let mut c = col().w_full().gap(8.0).child(grid).child(row().gap(10.0).center_y().child(octal)
        .child(text(symbolic.clone()).key("perm-symbolic").font(k.ty.mono).color(k.c.text_muted).label(format!("Symbolic: {symbolic}"))));
    if let Some(e) = &p.octal_error {
        c = c.child(text(e.clone()).key("perm-octal-error").font(k.ty.text_xsmall).color(paint.danger).wrap().live_region(rune::LiveRegion::Polite));
    }
    if any_dir {
        // The label wraps beside the box (a narrow panel can't fit it on one line); a click on
        // either toggles it, and the box carries the label for screen readers.
        let flip = |s: &mut Ui| s.inspector.perms.folders_x = !s.inspector.perms.folders_x;
        let label = "Folders get execute where read is set";
        c = c.child(row().key("perm-folders-x-row").w_full().gap(8.0).items_start()
            .help("For Apply to Enclosed, like chmod's X: wherever your change allows reading a folder, it also allows opening it (execute), so folders stay browsable. Files get exactly the change.")
            .child(Checkbox::new(p.folders_x).label(label).hide_label().disabled(!enabled).on_toggle(flip).build(k).key("perm-folders-x").pt(2.0))
            .child(text(label).font(k.ty.text_small).color(k.c.text_base).wrap().grow(1.0).min_w(0.0).a11y_hidden()
                .on_press(move |s: &mut Ui| if enabled { flip(s) })));
    }
    c.child(perm_actions(i, modes, any_dir, k, paint))
}

fn perm_actions(i: &Inspector, modes: &[u32], any_dir: bool, k: &Kit, paint: &Paint) -> El {
    let p = &i.perms;
    if let Some(a) = &p.applying {
        let (done, failed) = (a.tally.done.load(std::sync::atomic::Ordering::Relaxed), a.tally.failed.load(std::sync::atomic::Ordering::Relaxed));
        let what = "Applying";
        let more = if failed > 0 { format!(", {} failed", fmt::count(failed)) } else { String::new() };
        return row().key("perm-applying").w_full().gap(8.0).center_y()
            .child(Spinner::new().size(14.0).build(k).a11y_hidden())
            .child(text(format!("{what}… {} done{more}", fmt::count(done))).font(k.ty.text_small).color(k.c.text_muted).grow(1.0).min_w(0.0).wrap().live_region(rune::LiveRegion::Polite))
            .child(link(k, paint, "perm-stop", "Stop", |s: &mut Ui| s.cancel_apply()));
    }
    let ready = p.dirty(modes) && p.octal_error.is_none();
    let primary = crate::theme::primary(k);
    let mut buttons = row().w_full().gap(6.0).center_y().flex_wrap()
        .child(Button::new("Apply").variant(ButtonVariant::Contrast).control_size(ControlSize::Small).disabled(!ready)
            .on_press(|s: &mut Ui| s.request_apply(false)).build(&primary).key("perm-apply"));
    if any_dir {
        buttons = buttons.child(Button::new("Apply to Enclosed").variant(ButtonVariant::Neutral).control_size(ControlSize::Small).disabled(!ready)
            .help("Also change everything inside the selected folders (runs in Transfers)")
            .on_press(|s: &mut Ui| s.request_apply(true)).build(k).key("perm-apply-enclosed"));
    }
    if !p.staged.is_empty() {
        buttons = buttons.child(link(k, paint, "perm-revert", "Revert", |s: &mut Ui| s.revert_perms()));
    }
    let mut c = col().w_full().gap(6.0).child(buttons);
    if let Some(e) = &p.error {
        c = c.child(failure(k, paint, "perm-error", e, |s: &mut Ui| s.apply_perms(false)));
    }
    if let Some(n) = &p.note {
        c = c.child(text(n.clone()).key("perm-note").font(k.ty.text_xsmall).color(k.c.text_muted).live_region(rune::LiveRegion::Polite));
    }
    c
}

/// Owner and group, read-only.
pub fn owner_group(k: &Kit, owner: Option<&String>, group: Option<&String>) -> El {
    let v = |x: Option<&String>| x.cloned().unwrap_or_else(|| "n/a".into());
    col().w_full().gap(4.0)
        .child(fact(k, "owner", "Owner", v(owner), owner.is_none()))
        .child(fact(k, "group", "Group", v(group), group.is_none()))
}

/// The kind of an entry for a summary ("Folder", "PNG image").
pub fn kind_name(e: &filewinch_core::Entry) -> String {
    if e.kind == EntryKind::Dir { "Folder".into() } else { crate::pane::model::file_kind(e).describe(&e.name) }
}

/// The large icon at the top of the panel.
pub fn big_icon(e: &filewinch_core::Entry, k: &Kit, paint: &Paint) -> El {
    let kind = crate::pane::model::file_kind(e);
    icon(kind.icon(), 40.0).color(kind.tint(paint.folder, k.c.icon_muted, paint.dark)).a11y_hidden()
}

/// A colour for the Shared badge.
pub fn shared_tint(paint: &Paint) -> Color {
    paint.accent
}
