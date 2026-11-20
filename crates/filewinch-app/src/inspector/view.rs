//! The panel: docked at the window's right edge (resizable, 240–420 pt), or floating over the
//! panes when the window is too narrow for both; what it shows for one item, several, or none;
//! and its questions ("Apply changes?", bulk Apply, Apply to Enclosed).

use filewinch_core::{EntryKind, HashAlgo};
use rune::{Cursor, Drag, El, Phase, ScrollSpec, Tx, col, el, icon, row, text};
use rune_kit::{Button, ButtonVariant, Dialog, IconButton, IconButtonVariant, Kit, Spinner};

use super::parts::{self, copy_link, date_fact, fact, fact_with, link, note, section};
use super::perms::Confirm;
use super::{Load, Preview, Section, Size, Subject};
use crate::fmt;
use crate::theme::Paint;
use crate::ui::Ui;

/// Where Tab from the right pane lands.
pub const FOCUS_KEY: &str = "inspector";

/// The toolbar button: a window with a panel at its right.
pub static ICON: rune::Icon = rune::Icon {
    name: "fw-inspector",
    view: 16.0,
    body: r#"<rect x="1.75" y="2.75" width="12.5" height="10.5" rx="1.75" stroke="currentColor" stroke-width="1.2" fill="none"/><path d="M9.75 3V13" stroke="currentColor" stroke-width="1.2"/><path d="M11.4 5.75H12.6M11.4 8H12.6M11.4 10.25H12.6" stroke="currentColor" stroke-width="1.1" stroke-linecap="round"/>"#,
};

/// Room the two panes need before the panel floats instead of squeezing them.
const PANES_MIN: f32 = 2.0 * 300.0 + 24.0;
const SIDEBAR_W: f32 = 236.0;

pub fn algo_name(a: HashAlgo) -> &'static str {
    match a {
        HashAlgo::Md5 => "MD5",
        HashAlgo::Sha1 => "SHA-1",
        HashAlgo::Sha256 => "SHA-256",
        HashAlgo::DropboxContent => "Dropbox hash",
        HashAlgo::QuickXor => "QuickXorHash",
    }
}

/// Whether the panel floats over the panes at this width (window content `main_w`).
pub fn floats(main_w: f32, width: f32, sidebar: bool) -> bool {
    main_w < super::OVERLAY_BELOW || main_w - width - if sidebar { SIDEBAR_W } else { 0.0 } < PANES_MIN
}

/// Adds the panel to the window's main row (after the panes), and its questions to `layer`.
pub fn attach(s: &Ui, main: El, k: &Kit, paint: &Paint, layer: &mut Vec<El>) -> El {
    let main = main.on_layout(|s: &mut Ui, r: [f32; 4]| s.inspector.main_w = r[2]);
    if !s.prefs.inspector_open {
        return main;
    }
    // (Not while "Apply changes?" holds the old selection: nothing would follow until it's answered.)
    if !s.inspector.prompt && !s.inspector_current() {
        // The selection changed in this frame's input: follow it on the next frame.
        s.inspector.nudge();
    }
    let w = s.prefs.inspector_width as f32;
    let floating = floats(s.inspector.main_w, w, s.prefs.sidebar_open);
    dialogs(s, k, layer);
    let panel = panel(s, k, paint, w, floating);
    if floating {
        main.child(k.elevate(panel.pin(None, Some(0.0), Some(0.0), Some(0.0)), &k.elevation.floating))
    } else {
        main.child(panel)
    }
}

fn panel(s: &Ui, k: &Kit, paint: &Paint, w: f32, floating: bool) -> El {
    let handle = el().key("inspector-resize").role("separator").label("Inspector width").focusable(true)
        .pin(Some(0.0), Some(0.0), None, Some(0.0)).w(6.0).cursor(Cursor::ResizeColumn)
        .help("Drag to resize the Inspector")
        .on_drag(|s: &mut Ui, d: &mut Drag, _tx: &mut Tx| match d.phase {
            Phase::Down => d.data = s.prefs.inspector_width as f32,
            Phase::Move => s.set_inspector_width(d.data - d.dx, false),
            Phase::Up => s.set_inspector_width(if d.cancelled() { d.data } else { d.data - d.dx }, true),
        })
        .on_key(|s: &mut Ui, ev: &rune::KeyEvent, _tx: &mut Tx| {
            let step = match ev.key {
                rune::Key::Left => 20.0,
                rune::Key::Right => -20.0,
                _ => return false,
            };
            s.set_inspector_width(s.prefs.inspector_width as f32 + step, true);
            true
        });
    let copy = s.inspector.subject.as_ref().map(|subj| {
        let text = super::summary(subj, s.inspector.details.ready(), &s.inspector.size, fmt::now());
        IconButton::new(k.glyph.copy).variant(IconButtonVariant::Ghost).label("Copy info").help("Copy these details as text")
            .on_press_tx(move |_: &mut Ui, tx: &mut Tx| tx.copy(text.clone())).build(k).key("inspector-copy")
    });
    let mut head = row().w_full().h(36.0).pl(14.0).pr(6.0).gap(2.0).center_y()
        .child(text("Inspector").font(k.ty.text_small_medium).color(k.c.text_strong).role("heading").level(2).grow(1.0));
    if let Some(c) = copy {
        head = head.child(c);
    }
    head = head.child(IconButton::new(k.glyph.close).variant(IconButtonVariant::Ghost).label("Close Inspector")
        .on_press(|s: &mut Ui| s.toggle_inspector()).build(k).key("inspector-close"));
    col().key(FOCUS_KEY).role("complementary").label("Inspector").focusable(true)
        .w(w).h_full().min_h(0.0).fill(if floating { k.c.surface_float } else { paint.chrome })
        .border_sides([0.0, 0.0, 0.0, 1.0], paint.hair)
        .on_key(|s: &mut Ui, ev: &rune::KeyEvent, tx: &mut Tx| {
            if ev.key == rune::Key::Escape {
                s.inspector_escape(tx);
                return true;
            }
            false
        })
        .child(head)
        .child(col().key("inspector-body").w_full().grow(1.0).min_h(0.0).scroll(ScrollSpec { stick_bottom: false })
            .child(col().w_full().gap(10.0).px(12.0).pt(4.0).pb(16.0).child(body(s, k, paint))))
        .child(handle)
}

fn body(s: &Ui, k: &Kit, paint: &Paint) -> El {
    let Some(subj) = &s.inspector.subject else {
        let why = if s.panes[s.active].is_empty() { "Connect to a server or open a folder to see its details here." } else { "Loading…" };
        return col().key("inspector-empty").w_full().pt(24.0).items_center().child(note(k, why));
    };
    match subj.items.len() {
        0 => folder(s, subj, k, paint),
        1 => one(s, subj, k, paint),
        _ => many(s, subj, k, paint),
    }
}

/// Icon, name (renamable), kind, and the Shared badge.
fn identity(s: &Ui, subj: &Subject, e: &filewinch_core::Entry, k: &Kit, paint: &Paint, renamable: bool) -> El {
    let name: El = match &s.inspector.rename {
        Some(r) if renamable => {
            let input = rune_kit::TextInput::new(&r.edit).label(format!("New name for {}", e.name)).width(None).invalid(r.error.is_some()).disabled(r.busy)
                .on_edit(|s: &mut Ui| s.inspector_rename_edit())
                .on_submit(|s: &mut Ui, _tx: &mut Tx| s.inspector_commit_rename())
                .on_key(|s: &mut Ui, ev: &rune::KeyEvent, _tx: &mut Tx| {
                    if ev.key == rune::Key::Escape {
                        s.inspector.rename = None;
                        return true;
                    }
                    false
                })
                .build(k).key("inspector-rename");
            let mut c = col().w_full().gap(2.0).child(input);
            if let Some(err) = &r.error {
                c = c.child(text(err.clone()).key("inspector-rename-error").font(k.ty.text_xsmall).color(paint.danger).wrap().live_region(rune::LiveRegion::Polite));
            }
            c
        }
        _ => text(e.name.clone()).key("inspector-name").font(k.ty.ui_medium).color(k.c.text_strong).wrap().selectable(true).role("heading").level(3),
    };
    let mut words = col().grow(1.0).min_w(0.0).gap(2.0).items_start().child(name)
        .child(text(parts::kind_name(e)).key("inspector-kind").font(k.ty.text_small).color(k.c.text_muted));
    if renamable && s.inspector.rename.is_none() && subj.caps.server_rename {
        words = words.child(parts::link_tx(k, paint, "inspector-rename-start", "Rename", |s: &mut Ui, tx: &mut Tx| s.inspector_begin_rename(tx)));
    }
    let mut c = col().w_full().gap(8.0).child(row().w_full().gap(10.0).items_start().child(parts::big_icon(e, k, paint)).child(words));
    if let Some(sh) = e.shared {
        let why = match (sh.owned_by_me, sh.can_leave) {
            (true, _) => "You share this with other people. Deleting it deletes it for everyone it's shared with.",
            (false, true) => "Someone shared this with you. You can remove it from your account without deleting it for them.",
            (false, false) => "Someone shared this with you. Deleting it may delete it for everyone it's shared with.",
        };
        let tint = parts::shared_tint(paint);
        c = c.child(col().key("inspector-shared").role("note").label(format!("Shared. {why}")).w_full().gap(4.0).padding(8.0).radius(6.0)
            .fill(k.alpha(tint, 0.08)).border(1.0, k.alpha(tint, 0.3))
            .child(row().gap(6.0).center_y().child(icon(&crate::icons::SHARED, 14.0).color(tint).a11y_hidden())
                .child(text("Shared").font(k.ty.text_xsmall_medium).color(tint).a11y_hidden()))
            .child(text(why).font(k.ty.text_xsmall).color(k.c.text_base).wrap().a11y_hidden()));
    }
    c
}

/// The Size fact for a folder (or the folders in a selection): Calculate, progress with
/// Cancel, the total, or what went wrong with Retry.
fn folder_size(s: &Ui, k: &Kit, paint: &Paint, label: &str, files_too: u64) -> El {
    let calc = |title: &str| link(k, paint, "inspector-calculate", title, |s: &mut Ui| s.calculate_size(false));
    let value = |v: String| text(v).key("inspector-size").font(k.ty.text_small).color(k.c.text_base).wrap().selectable(true);
    match &s.inspector.size {
        Size::NotAsked => fact_with(k, "size", label, value("—".into()).label(format!("{label}: not calculated")), calc("Calculate Size")),
        Size::TooMany => fact_with(k, "size", label, value("Large folder".into()), calc("Calculate Size")),
        Size::Counting(t) => {
            let (bytes, files, _) = t.get();
            let progress = format!("Counting… {} in {} files", fmt::size(bytes + files_too), fmt::count(files));
            fact_with(k, "size", label,
                row().gap(6.0).center_y().child(Spinner::new().size(12.0).build(k).a11y_hidden())
                    .child(text(progress).font(k.ty.text_small).color(k.c.text_muted).live_region(rune::LiveRegion::Polite)),
                link(k, paint, "inspector-calc-cancel", "Cancel", |s: &mut Ui| s.cancel_size()))
        }
        Size::Done { bytes, files, folders } => {
            let total = bytes + files_too;
            fact(k, "size", label, format!("{} ({}) in {} files, {} folders", fmt::size(total), fmt::exact_bytes(total), fmt::count(*files), fmt::count(*folders)), false)
        }
        Size::Failed(e) => fact_with(k, "size", label, value("Couldn't calculate".into()),
            parts::failure(k, paint, "inspector-size-error", &e.message, |s: &mut Ui| s.calculate_size(false))),
    }
}

/// "18 KB (18,420 bytes)", or just "438 bytes" when that's already exact.
fn size_text(n: u64) -> String {
    if n < 1000 { fmt::size(n) } else { format!("{} ({})", fmt::size(n), fmt::exact_bytes(n)) }
}

/// Path and URL, each with Copy.
fn where_facts(subj: &Subject, path: &filewinch_core::RPath, k: &Kit, paint: &Paint) -> El {
    let url = crate::urls::url_for(subj.site.as_ref(), path);
    // Long paths and URLs have no spaces to wrap at: break them anywhere.
    let mono = |key: &str, t: String| text(t).key(key.to_owned()).font(k.ty.text_small).color(k.c.text_base).wrap_anywhere().selectable(true);
    col().w_full().gap(6.0)
        .child(fact_with(k, "path", "Path", mono("inspector-path", path.to_string()).label(format!("Path: {path}")), copy_link(k, paint, "copy-path", "Path", path.to_string())))
        .child(fact_with(k, "url", "URL", mono("inspector-url", url.clone()).label(format!("URL: {url}")), copy_link(k, paint, "copy-url", "URL", url)))
}

/// `who`: the one item's owner and group (`None` for several items, which may differ).
fn perms_section(s: &Ui, any_dir: bool, who: Option<(Option<&String>, Option<&String>)>, k: &Kit, paint: &Paint) -> Option<El> {
    // Hidden entirely where the backend has no modes (S3, Azure, cloud drives).
    let modes = s.inspector.modes()?;
    Some(section(s, k, paint, Section::Permissions, || {
        let mut c = col().w_full().gap(10.0).child(parts::perms_editor(&s.inspector, &modes, any_dir, k, paint));
        if let Some((owner, group)) = who {
            c = c.child(parts::owner_group(k, owner, group));
        }
        c
    }))
}

fn one(s: &Ui, subj: &Subject, k: &Kit, paint: &Paint) -> El {
    let e = &subj.items[0];
    let i = &s.inspector;
    let dates = i.details.ready().map(|d| d.dates).unwrap_or_default();
    let mut c = col().w_full().gap(12.0).child(identity(s, subj, e, k, paint, true));
    if !matches!(i.preview, Preview::None) {
        c = c.child(section(s, k, paint, Section::Preview, || parts::preview(i, &e.name, k, paint)));
    }
    c = c.child(section(s, k, paint, Section::Info, || {
        let size = if e.kind == EntryKind::Dir {
            folder_size(s, k, paint, "Size", 0)
        } else {
            match e.size {
                Some(n) => fact(k, "size", "Size", size_text(n), false),
                None => fact(k, "size", "Size", "n/a", true),
            }
        };
        let mut f = col().w_full().gap(6.0).child(size)
            .child(date_fact(k, "created", "Created", dates.created))
            .child(date_fact(k, "modified", "Modified", e.modified))
            .child(date_fact(k, "accessed", "Accessed", dates.accessed));
        if let Some(t) = &e.link_target {
            f = f.child(fact(k, "target", "Points to", t.clone(), false));
        }
        if let Load::Failed(err) = &i.details {
            f = f.child(parts::failure(k, paint, "inspector-details-error", &err.message, |s: &mut Ui| s.inspector_details()));
        }
        f.child(where_facts(subj, &e.path, k, paint))
    }));
    if let Some(p) = perms_section(s, e.kind == EntryKind::Dir, Some((e.owner.as_ref(), e.group.as_ref())), k, paint) {
        c = c.child(p);
    }
    if !e.checksums.is_empty() {
        c = c.child(section(s, k, paint, Section::Checksums, || {
            let mut l = col().w_full().gap(8.0);
            for (n, sum) in e.checksums.iter().enumerate() {
                let name = algo_name(sum.algo);
                l = l.child(col().key(format!("checksum-{n}")).w_full().gap(2.0).items_start().label(format!("{name}: {}", sum.hex))
                    .child(text(name).font(k.ty.text_xsmall).color(k.c.text_muted).a11y_hidden())
                    .child(text(sum.hex.clone()).font(k.ty.mono).color(k.c.text_base).wrap_anywhere().selectable(true).a11y_hidden())
                    .child(copy_link(k, paint, &format!("copy-checksum-{n}"), name, sum.hex.clone())));
            }
            l
        }));
    }
    if !e.extra.is_empty() {
        c = c.child(section(s, k, paint, Section::Details, || {
            let mut l = col().w_full().gap(6.0);
            for (n, (name, value)) in e.extra.iter().enumerate() {
                l = l.child(fact(k, &format!("extra-{n}"), name, value.clone(), false));
            }
            l
        }));
    }
    c
}

/// "4 PNG image, 3 Folder, 2 HTML source and 3 more kinds" → as lines, most common first.
pub fn kinds(items: &[filewinch_core::Entry]) -> Vec<(String, usize)> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for e in items {
        let k = parts::kind_name(e);
        match counts.iter_mut().find(|(n, _)| *n == k) {
            Some(c) => c.1 += 1,
            None => counts.push((k, 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts
}

fn many(s: &Ui, subj: &Subject, k: &Kit, paint: &Paint) -> El {
    let n = subj.items.len();
    let files: Vec<_> = subj.items.iter().filter(|e| e.kind != EntryKind::Dir).collect();
    let dirs = n - files.len();
    let file_bytes: u64 = files.iter().filter_map(|e| e.size).sum();
    let head = row().w_full().gap(10.0).center_y()
        .child(icon(&crate::icons::FILE, 40.0).color(k.c.icon_muted).a11y_hidden())
        .child(col().gap(2.0)
            .child(text(format!("{} items", fmt::count(n as u64))).key("inspector-count").font(k.ty.ui_medium).color(k.c.text_strong).role("heading").level(3))
            .child(text(format!("in {}", if subj.dir.is_root() { "/" } else { subj.dir.name() })).font(k.ty.text_small).color(k.c.text_muted)));
    let mut c = col().w_full().gap(12.0).child(head);
    c = c.child(section(s, k, paint, Section::Info, || {
        let mut f = col().w_full().gap(6.0)
            .child(fact(k, "files", "Files", format!("{} in {} files", fmt::size(file_bytes), fmt::count(files.len() as u64)), false));
        if dirs > 0 {
            f = f.child(match &s.inspector.size {
                Size::NotAsked | Size::TooMany => fact_with(k, "size", "Folders", text(format!("{} folders", fmt::count(dirs as u64))).font(k.ty.text_small).color(k.c.text_base),
                    link(k, paint, "inspector-calculate", "Calculate folder sizes", |s: &mut Ui| s.calculate_size(false))),
                _ => folder_size(s, k, paint, "Total", file_bytes),
            });
        }
        let list = kinds(&subj.items);
        let mut summary: Vec<String> = list.iter().take(4).map(|(name, c)| format!("{c} × {name}")).collect();
        if list.len() > 4 {
            summary.push(format!("{} more kinds", list.len() - 4));
        }
        f.child(fact(k, "kinds", "Kinds", summary.join("\n"), false))
    }));
    let any_dir = dirs > 0;
    if let Some(p) = perms_section(s, any_dir, None, k, paint) {
        c = c.child(p);
    }
    c
}

/// Nothing selected: the folder the pane is in.
fn folder(s: &Ui, subj: &Subject, k: &Kit, paint: &Paint) -> El {
    let i = &s.inspector;
    let entry = i.folder().cloned().unwrap_or_else(|| filewinch_core::Entry {
        name: if subj.dir.is_root() { "/".into() } else { subj.dir.name().to_owned() },
        path: subj.dir.clone(),
        kind: EntryKind::Dir,
        size: None,
        modified: None,
        mode: None,
        owner: None,
        group: None,
        link_target: None,
        extra: Vec::new(),
        checksums: Vec::new(),
        shared: None,
    });
    let dates = i.details.ready().map(|d| d.dates).unwrap_or_default();
    let mut c = col().w_full().gap(12.0).child(identity(s, subj, &entry, k, paint, false));
    c = c.child(section(s, k, paint, Section::Info, || {
        let mut f = col().w_full().gap(6.0)
            .child(fact(k, "items", "Contains", if subj.count == 1 { "1 item".into() } else { format!("{} items", fmt::count(subj.count as u64)) }, false))
            .child(folder_size(s, k, paint, "Size", 0))
            .child(date_fact(k, "created", "Created", dates.created))
            .child(date_fact(k, "modified", "Modified", entry.modified))
            .child(date_fact(k, "accessed", "Accessed", dates.accessed));
        match &i.details {
            Load::Failed(err) => f = f.child(parts::failure(k, paint, "inspector-details-error", &err.message, |s: &mut Ui| s.inspector_details())),
            Load::Loading => f = f.child(row().gap(6.0).center_y().child(Spinner::new().size(12.0).build(k).label("Loading folder details"))),
            _ => {}
        }
        f.child(where_facts(subj, &subj.dir, k, paint))
    }));
    if let Some(p) = perms_section(s, true, Some((entry.owner.as_ref(), entry.group.as_ref())), k, paint) {
        c = c.child(p);
    }
    c
}

/// "Apply changes to “x”?" before following a new selection, and the bulk / enclosed confirms.
fn dialogs(s: &Ui, k: &Kit, layer: &mut Vec<El>) {
    let i = &s.inspector;
    let Some(subj) = &i.subject else { return };
    let what = match subj.items.as_slice() {
        [] => format!("“{}”", if subj.dir.is_root() { "/" } else { subj.dir.name() }),
        [one] => format!("“{}”", one.name),
        many => format!("these {} items", fmt::count(many.len() as u64)),
    };
    let buttons = |ok: &str, ok_key: &str, ok_f: fn(&mut Ui), other: &str, other_key: &str, other_f: fn(&mut Ui)| {
        row().w_full().gap(8.0).child(el().grow(1.0))
            .child(Button::new(other).variant(ButtonVariant::Ghost).on_press(other_f).build(k).key(other_key.to_owned()))
            .child(Button::new(ok).variant(ButtonVariant::Contrast).on_press(ok_f).build(&crate::theme::primary(k)).key(ok_key.to_owned()).default_button())
    };
    let body = |message: String, actions: El| col().gap(14.0).px(16.0).pb(16.0).w_full()
        .child(text(message).font(k.ty.text_small).color(k.c.text_base).wrap()).child(actions);
    if i.prompt {
        let msg = format!("You changed the permissions of {what} but haven't applied them.");
        Dialog::new("inspector-prompt", format!("Apply changes to {what}?")).open(true).width(420.0).fit()
            .child(body(msg, buttons("Apply", "prompt-apply", |s| s.answer_prompt(true), "Discard", "prompt-discard", |s| s.answer_prompt(false))))
            .on_close(|s: &mut Ui| s.keep_editing()).build(k, layer);
        return;
    }
    let mode = i.modes().and_then(|m| super::perms::uniform(&m, i.perms.staged)).map(|m| format!(" to {} ({})", super::perms::octal(m), fmt::mode_string(m))).unwrap_or_default();
    match i.perms.confirm {
        None => {}
        Some(Confirm::Bulk) => {
            Dialog::new("inspector-confirm", "Change permissions?").open(true).width(420.0).fit()
                .child(body(format!("Change the permissions of {what}{mode}?"), buttons("Apply to All", "confirm-apply", |s| s.apply_perms(false), "Cancel", "confirm-cancel", |s| s.inspector.perms.confirm = None)))
                .on_close(|s: &mut Ui| s.inspector.perms.confirm = None).build(k, layer);
        }
        Some(Confirm::Enclosed) => {
            let x = if i.perms.folders_x { " Folders also get execute wherever the change allows reading them." } else { "" };
            // What a single folder will become, by the engine's own rule.
            let one = match (subj.items.as_slice(), i.modes().as_deref()) {
                ([e], Some([m])) if e.is_dir() => {
                    let new = i.perms.staged.enclosed(*m, true, i.perms.folders_x);
                    format!(" “{}” becomes {} ({}).", e.name, super::perms::octal(new), fmt::mode_string(new))
                }
                _ => String::new(),
            };
            Dialog::new("inspector-confirm", "Apply to enclosed items?").open(true).width(440.0).fit()
                .child(body(format!("Change the permissions of {what} and everything inside?{one}{x} Progress shows in Transfers."), buttons("Apply to Enclosed", "confirm-apply", |s| s.apply_perms(true), "Cancel", "confirm-cancel", |s| s.inspector.perms.confirm = None)))
                .on_close(|s: &mut Ui| s.inspector.perms.confirm = None).build(k, layer);
        }
    }
}
