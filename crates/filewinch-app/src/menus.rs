//! Context menus on rows and panes, window shortcuts, and the menu bar.

use filewinch_core::EntryKind;
use filewinch_os::Os;
use rune::{KeyEvent, MenuItem, PointerPress, PopupItem, PopupMenu, Tx};

use crate::connect::form::Form;
use crate::keys::Act;
use crate::ui::{Sheet, Ui};

fn item(id: &str, title: &str, key: Option<Act>) -> PopupItem {
    let mut i = PopupItem::new(id, title);
    if let Some(k) = key.and_then(Act::shortcut) {
        i = i.key_equivalent(k);
    }
    i
}

impl Ui {
    /// Right-click on a row: select it (unless it's in the selection) and show what applies.
    pub fn row_menu(&mut self, p: usize, name: &str, press: PointerPress, tx: &mut Tx) {
        self.active = p;
        if !self.panes[p].sel.is_selected(name) {
            self.panes[p].sel.only(name);
        }
        let menu = self.selection_menu(p);
        tx.popup_menu(menu, press, move |s: &mut Ui, picked, tx: &mut Tx| {
            if let Some(id) = picked {
                s.menu_pick(p, id, tx);
            }
        });
    }

    /// Right-click on the pane's empty space.
    pub fn pane_menu(&mut self, p: usize, press: PointerPress, tx: &mut Tx) {
        self.active = p;
        let pane = &self.panes[p];
        let menu = if pane.sel.names.is_empty() { self.folder_menu(p) } else { self.selection_menu(p) };
        tx.popup_menu(menu, press, move |s: &mut Ui, picked, tx: &mut Tx| {
            if let Some(id) = picked {
                s.menu_pick(p, id, tx);
            }
        });
    }

    fn folder_menu(&self, p: usize) -> PopupMenu {
        let caps = self.panes[p].src.as_ref().map(|s| s.caps()).unwrap_or_default();
        PopupMenu::new()
            .item(item("new-folder", "New Folder", Some(Act::NewFolder)))
            .item(item("new-file", "New File", None))
            .separator()
            .item(item("copy-path", "Copy Path", None))
            .item(item("copy-url", "Copy URL", None))
            .separator()
            .item(item("refresh", "Refresh", Some(Act::Refresh)))
            .item(PopupItem::new("hidden", "Show Hidden Files").checked(self.prefs.show_hidden))
            .item(PopupItem::new("perms", "Show Permissions").checked(self.prefs.show_permissions).enabled(caps.permissions || self.panes[p].is_local()))
    }

    fn selection_menu(&self, p: usize) -> PopupMenu {
        let pane = &self.panes[p];
        let sel = pane.selected();
        let one = sel.len() == 1;
        let files_only = sel.iter().all(|e| e.kind == EntryKind::File);
        let caps = pane.src.as_ref().map(|s| s.caps()).unwrap_or_default();
        let other = &self.panes[self.other(p)];
        let send = match (pane.is_local(), other.is_local()) {
            _ if other.is_empty() => "Copy to Other Pane".to_owned(),
            (true, false) => format!("Upload to {}", other.label()),
            (false, true) => format!("Download to {}", crate::theme::ui_os().this_computer()),
            _ => format!("Copy to Other Pane ({})", if other.dir.is_root() { "/" } else { other.dir.name() }),
        };
        let mut menu = PopupMenu::new()
            .item(item("open", "Open", Some(Act::Open)))
            .item(PopupItem::new("edit", "Edit in External Editor").enabled(one && files_only))
            .separator()
            .item(PopupItem::new("send", send).enabled(!other.is_empty()));
        if pane.is_local() {
            menu = menu.item(PopupItem::new("reveal", format!("Show in {}", crate::theme::ui_os().file_manager())).enabled(!sel.is_empty()));
        } else if !crate::drag::DRAG_OUT {
            // No dragging out to the file manager here: download to a folder you pick instead.
            menu = menu.item(PopupItem::new("download-to", "Download to…").enabled(!sel.is_empty()));
        }
        menu.separator()
            .item(item("rename", "Rename", None).enabled(one && caps.server_rename))
            .item(PopupItem::new("duplicate", "Duplicate"))
            .item(item("info", Act::Info.title(), Some(Act::Info)).enabled(one))
            .separator()
            .item(item("new-folder", "New Folder", Some(Act::NewFolder)))
            .item(item("new-file", "New File", None))
            .separator()
            .item(PopupItem::new("copy-path", if one { "Copy Path" } else { "Copy Paths" }))
            .item(PopupItem::new("copy-url", if one { "Copy URL" } else { "Copy URLs" }))
            .separator()
            .item(item("delete", "Delete…", Some(Act::Delete)))
            .separator()
            .item(item("refresh", "Refresh", Some(Act::Refresh)))
    }

    pub fn menu_pick(&mut self, p: usize, id: &str, tx: &mut Tx) {
        match id {
            "open" => self.open_selection(p),
            "edit" => {
                if let Some(e) = self.panes[p].selected().into_iter().next() {
                    self.open_file(p, &e.path, true);
                }
            }
            "send" => self.send_to_other(p),
            "reveal" => self.reveal_selection(p),
            "download-to" => self.download_to(p, tx),
            "rename" => self.begin_rename(p, tx),
            "duplicate" => self.duplicate(p),
            "info" => self.open_info(p),
            "new-folder" => self.new_item(p, true),
            "new-file" => self.new_item(p, false),
            "copy-path" | "copy-url" => {
                let text = self.paths_text(p, id == "copy-url");
                tx.copy(text);
            }
            "delete" => self.ask_delete(p),
            "refresh" => self.refresh_pane(p),
            "hidden" => {
                let on = !self.prefs.show_hidden;
                self.set_show_hidden(on);
            }
            "perms" => {
                self.prefs.show_permissions = !self.prefs.show_permissions;
                self.save_prefs();
            }
            _ => {}
        }
    }

    /// The selection's paths (or URLs), one per line; the folder's when nothing is selected.
    pub fn paths_text(&self, p: usize, url: bool) -> String {
        let pane = &self.panes[p];
        let mut paths: Vec<_> = pane.selected().into_iter().map(|e| e.path).collect();
        if paths.is_empty() {
            paths.push(pane.dir.clone());
        }
        let site = pane.session().and_then(|id| self.session(id)).map(|s| &s.site);
        paths.iter().map(|path| if url { crate::urls::url_for(site, path) } else { path.to_string() }).collect::<Vec<_>>().join("\n")
    }

    /// The connect sheet; its connection lands in an empty pane if there is one, else the
    /// active pane.
    pub fn open_connect(&mut self) {
        self.open_connect_to(None);
    }

    /// The connect sheet for pane `p` (its empty state, or its source menu).
    pub fn open_connect_in(&mut self, p: usize) {
        self.active = p;
        self.open_connect_to(Some(p));
    }

    fn open_connect_to(&mut self, pane: Option<usize>) {
        let mut form = Form::new(filewinch_core::Protocol::Sftp);
        form.suggestions = crate::hub::ssh_hosts();
        form.pane = pane;
        self.sheet = Some(Sheet::Connect(Box::new(form)));
    }

    /// Window shortcuts that work wherever focus is (unless a field takes the key first).
    pub fn window_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        if self.sheet.is_some() || !self.conn.is_idle() {
            return false;
        }
        let Some(a) = Act::for_key(ev, tx) else { return false };
        // Off macOS, Delete is a bare key: only a focused pane takes it (`pane_key`), so it
        // never deletes from the sidebar or the Transfers list.
        if a == Act::Delete && Os::CURRENT != Os::Mac {
            return false;
        }
        self.act(a, tx);
        true
    }

    /// Runs a shortcut or menu action on the active pane.
    pub fn act(&mut self, a: Act, tx: &mut Tx) {
        let p = self.active;
        match a {
            Act::NewWindow => crate::open_new_window(),
            Act::Connect => self.open_connect(),
            Act::NewFolder => self.new_item(p, true),
            Act::Info => self.open_info(p),
            Act::Delete => self.ask_delete(p),
            Act::Refresh => self.refresh_pane(p),
            Act::Hidden => {
                let on = !self.prefs.show_hidden;
                self.set_show_hidden(on);
            }
            Act::Transfers => self.prefs.transfers_open = !self.prefs.transfers_open,
            Act::Sidebar => self.prefs.sidebar_open = !self.prefs.sidebar_open,
            Act::Inspector => self.toggle_inspector(),
            Act::Back => self.go_back(p),
            Act::Forward => self.go_forward(p),
            Act::Up => self.go_up(p),
            Act::FocusLeft | Act::FocusRight => {
                self.active = usize::from(a == Act::FocusRight);
                tx.focus(format!("pane-{}", self.active), true);
            }
            Act::Open => self.open_selection(p),
            Act::Settings => crate::settings::open_settings(&self.prefs),
            Act::Quit => tx.quit(),
        }
    }

    /// The app menu off macOS (Windows and Linux have no menu bar for Rune to fill): the menu
    /// bar's items, with Settings/Preferences and Exit/Quit at the end.
    pub fn app_menu(&mut self, tx: &mut Tx) {
        let a = |act: Act| item(&format!("act:{act:?}"), act.title(), Some(act));
        let menu = PopupMenu::new()
            .item(a(Act::NewWindow))
            .item(a(Act::Connect))
            .separator()
            .item(a(Act::NewFolder))
            .item(PopupItem::new("new-file", "New File"))
            .item(a(Act::Info))
            .item(a(Act::Delete))
            .separator()
            .item(PopupItem::new("sync", "Synchronize Folders…").enabled(self.empty_pane().is_none()))
            .item(PopupItem::new("disconnect", "Disconnect").enabled(!self.panes[self.active].is_local() && !self.panes[self.active].is_empty()))
            .separator()
            .item(a(Act::Refresh))
            .item(a(Act::Hidden).checked(self.prefs.show_hidden))
            .item(PopupItem::new("perms", "Show Permissions").checked(self.prefs.show_permissions))
            .item(PopupItem::new("reset-columns", "Reset Columns"))
            .item(a(Act::Transfers).checked(self.prefs.transfers_open))
            .item(a(Act::Sidebar).checked(self.prefs.sidebar_open))
            .item(item("act:Inspector", if self.prefs.inspector_open { "Hide Inspector" } else { "Show Inspector" }, Some(Act::Inspector)))
            .separator()
            .item(a(Act::Back))
            .item(a(Act::Forward))
            .item(a(Act::Up))
            .separator()
            .item(a(Act::Settings))
            .submenu("Help", PopupMenu::new().item(PopupItem::new("about", "About Filewinch")))
            .separator()
            .item(a(Act::Quit));
        tx.popup_menu(menu, "tb-menu", |s: &mut Ui, picked, tx: &mut Tx| {
            let Some(id) = picked else { return };
            let p = s.active;
            match id {
                "new-file" => s.new_item(p, false),
                "about" => crate::about::open_about(s),
                "sync" => s.open_sync(),
                "disconnect" => s.disconnect(p),
                "perms" => s.menu_pick(p, "perms", tx),
                "reset-columns" => {
                    s.prefs.columns = filewinch_store::ColumnPref::defaults();
                    s.save_prefs();
                }
                id => {
                    if let Some(a) = Act::WINDOW.into_iter().chain([Act::Open]).find(|a| id == format!("act:{a:?}")) {
                        s.act(a, tx);
                    }
                }
            }
        });
    }
}

/// The macOS menu bar. Items let the window's keys go first, so a focused field keeps its own
/// shortcuts (Cmd-Backspace in a text field deletes to the line start). Windows and Linux get
/// [`Ui::app_menu`] from the toolbar instead.
pub fn menu_bar() -> Vec<MenuItem<Ui>> {
    let mk = |menu: &str, a: Act, f: fn(&mut Ui, &mut Tx)| {
        let mut m = MenuItem::new(a.title(), f).menu(menu).keys_first();
        if let Some(k) = a.shortcut() {
            m = m.key_equivalent(k);
        }
        m
    };
    let plain = |title: &str, menu: &str, f: fn(&mut Ui, &mut Tx)| MenuItem::new(title, f).menu(menu).keys_first();
    let mut settings = MenuItem::new(Act::Settings.title(), |s: &mut Ui, tx: &mut Tx| s.act(Act::Settings, tx)).id("settings");
    if let Some(k) = Act::Settings.shortcut() {
        settings = settings.key_equivalent(k);
    }
    // Filewinch ▸ About Filewinch: first in the app menu (AppKit's own About is hidden at
    // launch, see `about::hide_standard_about_item`), then a separator before Settings.
    let about = MenuItem::new("About Filewinch", |s: &mut Ui, _: &mut Tx| crate::about::open_about(s)).id("about");
    vec![
        about,
        MenuItem::separator(),
        settings,
        mk("File", Act::NewWindow, |s, tx| s.act(Act::NewWindow, tx)),
        mk("File", Act::Connect, |s, tx| s.act(Act::Connect, tx)),
        MenuItem::separator().menu("File"),
        mk("File", Act::NewFolder, |s, tx| s.act(Act::NewFolder, tx)),
        plain("New File", "File", |s, _| s.new_item(s.active, false)),
        mk("File", Act::Info, |s, tx| s.act(Act::Info, tx)),
        mk("File", Act::Delete, |s, tx| s.act(Act::Delete, tx)),
        MenuItem::separator().menu("File"),
        plain("Synchronize Folders…", "File", |s, _| s.open_sync()),
        plain("Disconnect", "File", |s, _| s.disconnect(s.active)),
        mk("View", Act::Refresh, |s, tx| s.act(Act::Refresh, tx)),
        mk("View", Act::Hidden, |s, tx| s.act(Act::Hidden, tx)),
        plain("Show Permissions", "View", |s, _| {
            s.prefs.show_permissions = !s.prefs.show_permissions;
            s.save_prefs();
        }),
        plain("Reset Columns", "View", |s, _| {
            s.prefs.columns = filewinch_store::ColumnPref::defaults();
            s.save_prefs();
        }),
        mk("View", Act::Transfers, |s, tx| s.act(Act::Transfers, tx)),
        mk("View", Act::Sidebar, |s, tx| s.act(Act::Sidebar, tx)),
        mk("View", Act::Inspector, |s, tx| s.act(Act::Inspector, tx)),
        mk("Go", Act::Back, |s, tx| s.act(Act::Back, tx)),
        mk("Go", Act::Forward, |s, tx| s.act(Act::Forward, tx)),
        mk("Go", Act::Up, |s, tx| s.act(Act::Up, tx)),
    ]
}
