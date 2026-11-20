//! Deleting a selection that holds shared items (cloud folders other people can see). The safe
//! answer comes first: a shared item is *left* (taken out of this account; it stays for the
//! people it's shared with), and one that can't be left is kept. Deleting a share's contents
//! for everyone is a separate, secondary path that needs an explicit acknowledgement.

use filewinch_core::{Entry, RPath};
use rune::{El, col, row, text};
use rune_kit::{Button, ButtonVariant, Checkbox, ControlSize, Dialog, Kit};

use crate::theme::Paint;
use crate::ui::{Sheet, Ui};

/// One selected item: its path and name.
pub type Item = (RPath, String);

pub struct SharedDelete {
    pub pane: usize,
    /// The site's name, for the messages.
    pub site: String,
    /// Shared items this account can leave.
    pub leave: Vec<Item>,
    /// Shared items that can't be left (the user owns them, or the service won't let an app
    /// remove them): kept by the default action.
    pub keep: Vec<Item>,
    /// Ordinary items, deleted as usual.
    pub plain: Vec<Item>,
    /// `Some(acknowledged)` on the second step, "Delete contents for everyone".
    pub everyone: Option<bool>,
}

impl SharedDelete {
    /// Sorts a selection; `None` when nothing in it is shared (the ordinary confirm applies).
    pub fn from_selection(pane: usize, site: String, sel: Vec<Entry>) -> Option<SharedDelete> {
        if sel.iter().all(|e| e.shared.is_none()) {
            return None;
        }
        let mut d = SharedDelete { pane, site, leave: Vec::new(), keep: Vec::new(), plain: Vec::new(), everyone: None };
        for e in sel {
            let item = (e.path, e.name);
            match e.shared {
                Some(s) if s.can_leave => d.leave.push(item),
                Some(_) => d.keep.push(item),
                None => d.plain.push(item),
            }
        }
        Some(d)
    }

    fn shared_names(&self) -> Vec<&str> {
        self.leave.iter().chain(&self.keep).map(|i| i.1.as_str()).collect()
    }

    /// The default action's button, or None when it would do nothing (only kept items).
    pub fn primary_label(&self) -> Option<&'static str> {
        match (self.leave.is_empty(), self.plain.is_empty()) {
            (false, true) => Some("Leave"),
            (false, false) => Some("Leave and Delete"),
            (true, false) => Some("Delete Others"),
            (true, true) => None,
        }
    }

    pub fn title(&self) -> String {
        match (self.leave.as_slice(), self.keep.is_empty(), self.plain.is_empty()) {
            ([one], true, true) => format!("Leave “{}”?", one.1),
            (l, true, true) => format!("Leave {} shared items?", l.len()),
            _ => "Delete with shared items?".into(),
        }
    }

    /// What the default action does, one plain sentence per group.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        match self.leave.as_slice() {
            [] => {}
            [_] if self.keep.is_empty() && self.plain.is_empty() => out.push("It stays for the people it's shared with.".into()),
            [one] => out.push(format!("“{}” will be removed from your account. It stays for the people it's shared with.", one.1)),
            many => out.push(format!(
                "{} will be removed from your account. They stay for the people they're shared with.",
                quoted(&many.iter().map(|i| i.1.as_str()).collect::<Vec<_>>())
            )),
        }
        match self.keep.as_slice() {
            [] => {}
            [one] => out.push(format!(
                "“{}” is shared with others and can't be removed just for you, so it will be kept. If you own it, stop sharing it on the provider's website first.",
                one.1
            )),
            many => out.push(format!(
                "{} are shared with others and can't be removed just for you, so they will be kept.",
                quoted(&many.iter().map(|i| i.1.as_str()).collect::<Vec<_>>())
            )),
        }
        match self.plain.as_slice() {
            [] => {}
            [one] => out.push(format!("“{}” will be deleted from {}. This can't be undone.", one.1, self.site)),
            many => out.push(format!("The other {} items will be deleted from {}. This can't be undone.", many.len(), self.site)),
        }
        out
    }

    /// The second step's warning.
    pub fn everyone_text(&self) -> String {
        let names = self.shared_names();
        let what = if names.len() == 1 { format!("“{}” and everything in it", names[0]) } else { format!("{} and everything in them", quoted(&names)) };
        format!(
            "This deletes {what} for everyone it's shared with, not just for you. They lose these files too, and Filewinch can't undo it."
        )
    }
}

/// “a”, “b” and “c”
fn quoted(names: &[&str]) -> String {
    let q: Vec<String> = names.iter().map(|n| format!("“{n}”")).collect();
    match q.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

fn sheet_mut(s: &mut Ui) -> Option<&mut SharedDelete> {
    match &mut s.sheet {
        Some(Sheet::DeleteShared(d)) => Some(d),
        _ => None,
    }
}

impl Ui {
    /// The default action: leave what can be left, delete the ordinary items, keep the rest.
    pub fn delete_shared_default(&mut self) {
        let Some(Sheet::DeleteShared(d)) = self.sheet.take() else { return };
        let paths: Vec<RPath> = d.leave.into_iter().chain(d.plain).map(|i| i.0).collect();
        if !paths.is_empty() {
            self.delete_with(d.pane, paths, false);
        }
    }

    /// The confirmed destructive action: delete the shared items' contents for everyone.
    pub fn delete_shared_everyone(&mut self) {
        if !sheet_mut(self).is_some_and(|d| d.everyone == Some(true)) {
            return;
        }
        let Some(Sheet::DeleteShared(d)) = self.sheet.take() else { return };
        let paths: Vec<RPath> = d.leave.into_iter().chain(d.keep).chain(d.plain).map(|i| i.0).collect();
        self.delete_with(d.pane, paths, true);
    }
}

pub fn view(d: &SharedDelete, k: &Kit, paint: &Paint, layer: &mut Vec<El>) {
    let cancel = Button::new("Cancel").variant(ButtonVariant::Ghost).on_press(|s: &mut Ui| s.sheet = None).build(k).key("delete-cancel");
    let (title, body) = match d.everyone {
        None => {
            let mut lines = col().gap(8.0).w_full();
            for (i, l) in d.lines().into_iter().enumerate() {
                lines = lines.child(text(l).key(format!("line-{i}")).font(k.ty.text_small).color(k.c.text_base).wrap());
            }
            // Deliberately quiet and on the far side from the default button.
            let everyone = Button::new("Delete contents for everyone…").variant(ButtonVariant::Ghost).control_size(ControlSize::Small)
                .on_press(|s: &mut Ui| {
                    if let Some(d) = sheet_mut(s) {
                        d.everyone = Some(false);
                    }
                })
                .build(k).key("delete-everyone");
            let mut buttons = row().w_full().gap(8.0).center_y().child(everyone).child(rune::el().grow(1.0)).child(cancel);
            if let Some(label) = d.primary_label() {
                buttons = buttons.child(Button::new(label).variant(ButtonVariant::Contrast).on_press(|s: &mut Ui| s.delete_shared_default()).build(&crate::theme::primary(k)).key("delete-leave").default_button());
            }
            (d.title(), col().key("delete-shared-body").gap(16.0).px(16.0).pb(16.0).w_full().child(lines).child(buttons))
        }
        Some(ack) => {
            let warning = row().gap(10.0).items_start().w_full().padding(12.0).radius(8.0).fill(k.alpha(paint.danger, 0.08)).border(1.0, k.alpha(paint.danger, 0.35))
                .child(rune::icon(&crate::icons::WARNING, 16.0).color(paint.danger).a11y_hidden())
                .child(text(d.everyone_text()).key("everyone-text").font(k.ty.text_small).color(k.c.text_base).wrap().grow(1.0).min_w(0.0));
            let tick = Checkbox::new(ack).label("I understand everyone it's shared with will lose these files.")
                .on_toggle(|s: &mut Ui| {
                    if let Some(d) = sheet_mut(s)
                        && let Some(a) = &mut d.everyone
                    {
                        *a = !*a;
                    }
                })
                .build(k).key("everyone-ack");
            let back = Button::new("Back").variant(ButtonVariant::Ghost).on_press(|s: &mut Ui| {
                if let Some(d) = sheet_mut(s) {
                    d.everyone = None;
                }
            }).build(k).key("everyone-back");
            let go = Button::new("Delete for Everyone").variant(ButtonVariant::Danger).disabled(!ack)
                .on_press(|s: &mut Ui| s.delete_shared_everyone()).build(k).key("everyone-ok");
            let buttons = row().w_full().gap(8.0).center_y().child(back).child(rune::el().grow(1.0)).child(cancel).child(go);
            ("Delete for everyone?".to_owned(), col().key("delete-everyone-body").gap(14.0).px(16.0).pb(16.0).w_full().child(warning).child(tick).child(buttons))
        }
    };
    Dialog::new("delete-shared", &title).open(true).width(480.0).fit().child(body).on_close(|s: &mut Ui| s.sheet = None).build(k, layer);
}

#[cfg(test)]
mod tests {
    use super::*;
    use filewinch_core::{EntryKind, Shared};

    fn e(name: &str, shared: Option<Shared>) -> Entry {
        Entry {
            name: name.into(),
            path: RPath::new(&format!("/d/{name}")),
            kind: EntryKind::Dir,
            size: None,
            modified: None,
            mode: None,
            owner: None,
            group: None,
            link_target: None,
            extra: Vec::new(),
            checksums: Vec::new(),
            shared,
        }
    }
    const LEAVE: Option<Shared> = Some(Shared { owned_by_me: false, can_leave: true });
    const MINE: Option<Shared> = Some(Shared { owned_by_me: true, can_leave: false });

    #[test]
    fn ordinary_selections_use_the_plain_confirm() {
        assert!(SharedDelete::from_selection(0, "Drive".into(), vec![e("a", None)]).is_none());
    }

    #[test]
    fn one_leavable_share_reads_plainly() {
        let d = SharedDelete::from_selection(0, "Drive".into(), vec![e("Design", LEAVE)]).unwrap();
        assert_eq!(d.title(), "Leave “Design”?");
        assert_eq!(d.lines(), ["It stays for the people it's shared with."]);
        assert_eq!(d.primary_label(), Some("Leave"));
        assert_eq!(
            d.everyone_text(),
            "This deletes “Design” and everything in it for everyone it's shared with, not just for you. They lose these files too, and Filewinch can't undo it."
        );
    }

    #[test]
    fn mixed_selection_says_what_happens_to_each_group() {
        let d = SharedDelete::from_selection(
            0,
            "Drive".into(),
            vec![e("Design", LEAVE), e("Budget", MINE), e("a", None), e("b", None)],
        )
        .unwrap();
        assert_eq!(d.title(), "Delete with shared items?");
        let l = d.lines();
        assert_eq!(l.len(), 3);
        assert!(l[0].starts_with("“Design” will be removed from your account."));
        assert!(l[1].starts_with("“Budget” is shared with others and can't be removed just for you, so it will be kept."));
        assert_eq!(l[2], "The other 2 items will be deleted from Drive. This can't be undone.");
        assert_eq!(d.primary_label(), Some("Leave and Delete"));
        let only_kept = SharedDelete::from_selection(0, "Drive".into(), vec![e("Budget", MINE)]).unwrap();
        assert_eq!(only_kept.primary_label(), None, "nothing to do by default");
        assert_eq!(quoted(&["a", "b", "c"]), "“a”, “b” and “c”");
    }
}
