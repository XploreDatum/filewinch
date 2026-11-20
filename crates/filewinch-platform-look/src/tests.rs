use super::*;

#[test]
fn windows_prefers_uisettings_then_the_registry_then_fluent_blue() {
    let ui = [[1, 2, 3], [4, 5, 6], [7, 8, 9]];
    let a = windows_accent(Some(ui), Some(0xff00_00ff));
    assert_eq!(a, Accent { base: [1, 2, 3], light2: Some([4, 5, 6]), dark1: Some([7, 8, 9]) });
    // UISettings unavailable: the DWM value, ABGR, with shades left to the app.
    let a = windows_accent(None, Some(0xffd4_7800));
    assert_eq!(a, Accent { base: [0x00, 0x78, 0xd4], light2: None, dark1: None });
    assert_eq!(windows_accent(None, None), FLUENT_DEFAULT);
}

#[test]
fn dwm_colour_is_abgr() {
    assert_eq!(dwm_rgb(0xff11_2233), [0x33, 0x22, 0x11]);
    assert_eq!(dwm_rgb(0x0000_00ff), [0xff, 0, 0]);
}

#[test]
fn portal_accent_out_of_range_means_none() {
    assert_eq!(portal_accent(0.2078, 0.5176, 0.8941), Some([0x35, 0x84, 0xe4]));
    assert_eq!(portal_accent(0.0, 1.0, 0.5), Some([0, 255, 128]));
    for bad in [(-1.0, 0.5, 0.5), (0.5, 1.01, 0.5), (0.5, 0.5, f64::NAN), (2.0, 2.0, 2.0)] {
        assert_eq!(portal_accent(bad.0, bad.1, bad.2), None, "{bad:?}");
    }
}

#[test]
fn kde_is_found_in_the_desktop_list() {
    assert!(is_kde("KDE"));
    assert!(is_kde("plasma:kde"));
    assert!(!is_kde("ubuntu:GNOME"));
    assert!(!is_kde(""));
    assert!(!is_kde("KDEish"));
}

#[test]
fn set_reports_only_changes() {
    let before = version();
    set(Some(FLUENT_DEFAULT));
    let after_first = version();
    assert_eq!(after_first, before + 1);
    set(Some(FLUENT_DEFAULT));
    assert_eq!(version(), after_first, "the same accent again is not a change");
    assert_eq!(current(), Some(FLUENT_DEFAULT));
    set(None);
    assert_eq!(current(), None);
    assert_eq!(version(), after_first + 1);
}
