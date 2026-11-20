//! The paid licence in the app: the "free to use" banner, the Enter Licence Key sheet, the
//! background daily check, and keeping every window (and Settings and About) in step.
//!
//! Filewinch is free to use with every feature; registering only hides the banner. All the
//! rules (activate, check, deactivate, what un-registers) live in `filewinch-license`.

pub mod banner;
pub mod settings;
pub mod sheet;
#[cfg(test)]
mod tests;

use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use filewinch_license::{Check, Failure, Registration, Status, clean_key, device_label, now_secs};
use rune::TextEdit;

use crate::hub::{hub, sandboxed};
use crate::ui::{Notice, Sheet, Tone, Ui};

/// Where the Enter Licence Key sheet is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    /// The pasted text isn't a key (checked locally, no network).
    Invalid(&'static str),
    Activating,
    Failed(Failure),
}

pub struct LicenseSheet {
    pub key: TextEdit,
    pub phase: Phase,
}

impl LicenseSheet {
    pub fn new() -> LicenseSheet {
        LicenseSheet { key: TextEdit::new(""), phase: Phase::Idle }
    }

    /// The error under the field, if any.
    pub fn error(&self) -> Option<String> {
        match &self.phase {
            Phase::Invalid(m) => Some((*m).to_owned()),
            Phase::Failed(f) => Some(f.message()),
            Phase::Idle | Phase::Activating => None,
        }
    }
}

/// The status a window starts with. Headless runs and tests start registered to the scenes'
/// sample person, so screenshots and tests of everything else stay as they were; the
/// licence's own scenes and tests set what they need.
pub fn initial() -> Status {
    if sandboxed() { Status::Registered(sample()) } else { hub().licensing.status() }
}

/// The registration scenes show.
pub fn sample() -> Registration {
    Registration {
        name: Some("Alex Morgan".into()),
        email: Some("alex@example.com".into()),
        masked_key: "FW-••••-E304DA".into(),
        // The device name as the platform the scene is drawn for would report it.
        label: match crate::theme::ui_os() {
            filewinch_os::Os::Mac => "Filewinch on Alex's MacBook Pro",
            filewinch_os::Os::Windows => "Filewinch on ALEX-LAPTOP",
            filewinch_os::Os::Linux => "Filewinch on alex-laptop",
        }
        .into(),
        activated_at: 1_795_000_000,
        last_validated: 1_795_000_000,
        last_checked: 1_795_000_000,
    }
}

/// "Registered to Alex Morgan (alex@example.com)" or "Unregistered".
pub fn status_line(s: &Status) -> String {
    match s {
        Status::Unregistered => "Unregistered".into(),
        Status::Registered(r) => match (&r.name, &r.email) {
            (Some(n), Some(e)) => format!("Registered to {n} ({e})"),
            (Some(x), None) | (None, Some(x)) => format!("Registered to {x}"),
            (None, None) => "Registered".into(),
        },
    }
}

/// Hands a new status to every main window, Settings and About.
pub fn broadcast(status: Status) {
    let st = status.clone();
    hub().each(move |inbox| {
        let st = st.clone();
        inbox.post(move |ui| ui.license = st);
    });
    crate::settings::push_license(status.clone());
    crate::about::push_license(status);
}

static SHEET_ON_START: AtomicBool = AtomicBool::new(false);

/// Opens the Enter Licence Key sheet in the first open main window, or in a new one.
pub fn request_sheet() {
    match hub().first() {
        Some(inbox) => inbox.post(|ui| ui.open_license_sheet()),
        None => {
            SHEET_ON_START.store(true, Ordering::Relaxed);
            crate::open_new_window();
        }
    }
}

/// A window starting after [`request_sheet`] found none open takes the sheet.
pub fn take_sheet_request() -> bool {
    SHEET_ON_START.swap(false, Ordering::Relaxed)
}

/// Starts the background check once per process, after the first frame: now (at most daily,
/// the crate throttles), then every hour to catch a day passing while the app stays open.
pub fn start_checks() {
    static ONCE: Once = Once::new();
    if sandboxed() {
        return;
    }
    ONCE.call_once(|| {
        let _ = std::thread::Builder::new().name("filewinch-licence".into()).spawn(|| {
            loop {
                match hub().licensing.check(now_secs()) {
                    Check::Confirmed(r) => broadcast(Status::Registered(r)),
                    Check::Revoked => {
                        broadcast(Status::Unregistered);
                        if let Some(inbox) = hub().first() {
                            inbox.post(|ui| ui.licence_revoked());
                        }
                    }
                    Check::Unregistered | Check::NotDue | Check::Skipped(_) => {}
                }
                std::thread::sleep(Duration::from_secs(60 * 60));
            }
        });
    });
}

/// What the person sees when Polar says the licence is gone: calm, no dialog, stays until
/// closed.
pub const REVOKED: &str = "Your Filewinch licence is no longer active on this device, so the banner is back. Everything keeps working as before. If you didn't expect this, your customer portal has the details.";

impl Ui {
    pub fn open_license_sheet(&mut self) {
        self.sheet = Some(Sheet::License(Box::new(LicenseSheet::new())));
    }

    fn license_sheet(&mut self) -> Option<&mut LicenseSheet> {
        match &mut self.sheet {
            Some(Sheet::License(l)) => Some(l),
            _ => None,
        }
    }

    /// Activate pressed: check the text, then activate off the UI thread.
    pub fn activate_license(&mut self) {
        let inbox = self.inbox.clone();
        let Some(sheet) = self.license_sheet() else { return };
        if sheet.phase == Phase::Activating {
            return;
        }
        let key = match clean_key(&sheet.key.text) {
            Ok(k) => k,
            Err(m) => {
                sheet.phase = Phase::Invalid(m);
                return;
            }
        };
        sheet.phase = Phase::Activating;
        let spawned = std::thread::Builder::new().name("filewinch-activate".into()).spawn(move || {
            let result = hub().licensing.activate(&key, &device_label(), now_secs());
            inbox.post(move |ui| ui.license_activated(result));
        });
        if spawned.is_err()
            && let Some(sheet) = self.license_sheet()
        {
            sheet.phase = Phase::Failed(Failure::Offline("Couldn't start the activation.".into()));
        }
    }

    /// Activation's answer. Lands even if the sheet was closed meanwhile: a key that activated
    /// registers the app all the same.
    pub fn license_activated(&mut self, result: Result<Registration, Failure>) {
        match result {
            Ok(reg) => {
                if matches!(self.sheet, Some(Sheet::License(_))) {
                    self.sheet = None;
                }
                let who = reg.holder().map(|h| format!(" It's registered to {h}.")).unwrap_or_default();
                self.notify(Tone::Success, format!("Thank you for supporting Filewinch!{who}"), None);
                self.license = Status::Registered(reg.clone());
                broadcast(Status::Registered(reg));
            }
            Err(f) => match self.license_sheet() {
                Some(sheet) => sheet.phase = Phase::Failed(f),
                None => {
                    let detail = f.detail().map(str::to_owned);
                    self.notify(Tone::Error, format!("Filewinch couldn't activate your licence. {}", f.message()), detail);
                }
            },
        }
    }

    /// The daily check found the licence revoked: say so once, calmly.
    pub fn licence_revoked(&mut self) {
        self.license = Status::Unregistered;
        let id = self.next_id();
        self.notices.push(Notice { id, tone: Tone::Info, text: REVOKED.into(), detail: None, shown_detail: false, until: None });
    }
}
