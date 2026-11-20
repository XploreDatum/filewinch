//! Windows: `UISettings::GetColorValue` (Accent, AccentLight2, AccentDark1), followed through
//! `ColorValuesChanged`; the DWM registry value when WinRT doesn't answer, polled then, since
//! there's no notification without a window.

use std::time::Duration;

use windows::Foundation::TypedEventHandler;
use windows::UI::ViewManagement::{UIColorType, UISettings};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::core::w;

use crate::{Rgb, set, windows_accent};

/// How often the registry is reread when `UISettings` can't notify.
const POLL: Duration = Duration::from_secs(5);

pub(crate) fn run() {
    let settings = UISettings::new().ok();
    set(Some(windows_accent(settings.as_ref().and_then(ui_colors), dwm())));
    let subscribed = settings.as_ref().is_some_and(|s| {
        // Raised on an arbitrary thread pool thread; the handler only rereads and stores.
        let handler = TypedEventHandler::new(|sender: windows::core::Ref<UISettings>, _| {
            set(Some(windows_accent(sender.ok().ok().and_then(ui_colors), dwm())));
            Ok(())
        });
        s.ColorValuesChanged(&handler).is_ok()
    });
    loop {
        if subscribed {
            // `settings` must outlive the subscription: park this thread holding it.
            std::thread::park();
        } else {
            std::thread::sleep(POLL);
            set(Some(windows_accent(settings.as_ref().and_then(ui_colors), dwm())));
        }
    }
}

fn ui_colors(s: &UISettings) -> Option<[Rgb; 3]> {
    let get = |t: UIColorType| s.GetColorValue(t).ok().map(|c| [c.R, c.G, c.B]);
    Some([get(UIColorType::Accent)?, get(UIColorType::AccentLight2)?, get(UIColorType::AccentDark1)?])
}

/// `HKCU\Software\Microsoft\Windows\DWM\AccentColor`, `0xAABBGGRR`.
fn dwm() -> Option<u32> {
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: `value` and `size` outlive the call, and `size` says how much `value` holds.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\DWM"),
            w!("AccentColor"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&raw mut size),
        )
    };
    status.is_ok().then_some(value)
}
