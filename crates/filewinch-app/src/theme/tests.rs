use std::sync::Arc;

use filewinch_platform_look::{Accent, FLUENT_DEFAULT};
use rune::Color;

use super::tone::{self, contrast, flatten, rgb};
use super::*;

const LOOKS: [Look; 4] = [Look::MacOs, Look::Windows, Look::Gnome, Look::Kde];

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

#[test]
fn each_platform_gets_its_own_look() {
    assert_eq!(Look::native("macos", None), Look::MacOs);
    assert_eq!(Look::native("windows", Some("KDE")), Look::Windows);
    assert_eq!(Look::native("linux", None), Look::Gnome);
    assert_eq!(Look::native("linux", Some("ubuntu:GNOME")), Look::Gnome);
    assert_eq!(Look::native("linux", Some("KDE")), Look::Kde);
    assert_eq!(Look::native("freebsd", None), Look::Gnome, "other desktops get Adwaita");
    // The compiled target decides, absent `--look`.
    #[cfg(target_os = "macos")]
    assert_eq!(look(), Look::MacOs);
    #[cfg(target_os = "windows")]
    assert_eq!(look(), Look::Windows);
}

#[test]
fn look_flag_overrides_and_reports_unknown_names() {
    assert_eq!(look_arg(args(&["--snap", "a.png", "--look", "windows"])), Some(Ok(Look::Windows)));
    assert_eq!(look_arg(args(&["--look=linux"])), Some(Ok(Look::Gnome)));
    assert_eq!(look_arg(args(&["--look", "KDE"])), Some(Ok(Look::Kde)));
    assert_eq!(look_arg(args(&["--look", "macos", "--scene", "local"])), Some(Ok(Look::MacOs)));
    assert_eq!(look_arg(args(&["--look", "beos"])), Some(Err("beos".into())));
    assert_eq!(look_arg(args(&["--look"])), Some(Err(String::new())));
    assert_eq!(look_arg(args(&["--scene", "local"])), None);
}

#[test]
fn fluent_uses_windows_shades_and_derives_missing_ones() {
    let (light_bg, dark_bg, fg) = (rgb([0xfb; 3]), rgb([0x2b; 3]), rgb([0x1b; 3]));
    let light = tone::fluent(FLUENT_DEFAULT, false, light_bg, fg);
    let dark = tone::fluent(FLUENT_DEFAULT, true, dark_bg, Color::WHITE);
    assert_eq!(light.fill.hex(), "#005fb8ff", "AccentDark1 fills in light mode");
    assert_eq!(dark.fill.hex(), "#60cdffff", "AccentLight2 fills in dark mode");
    assert_eq!(light.on_fill, Color::WHITE);
    assert_eq!(dark.on_fill, Color::BLACK, "Fluent's dark accent buttons carry black text");
    assert_eq!(light.focus, fg, "Fluent focus strokes are the text colour");
    // The registry's base only: shades are derived, near Windows' own.
    let base_only = Accent { base: FLUENT_DEFAULT.base, light2: None, dark1: None };
    let derived = tone::fluent(base_only, false, light_bg, fg);
    assert!(oklab_distance(derived.fill, light.fill) < 0.04, "{}", derived.fill.hex());
    let derived = tone::fluent(base_only, true, dark_bg, Color::WHITE);
    assert!(oklab_distance(derived.fill, dark.fill) < 0.08, "{}", derived.fill.hex());
    for t in [light, dark] {
        assert!(contrast(t.on_fill, t.fill) >= 4.5);
    }
}

#[test]
fn adwaita_accent_text_reads_and_light_accents_get_dark_ink() {
    let white = Color::WHITE;
    let blue = tone::adwaita(rgb([0x35, 0x84, 0xe4]), white);
    assert_eq!(blue.on_fill, Color::WHITE);
    assert!(contrast(blue.text, white) >= 4.5, "accent_color is stepped to 4.5:1");
    assert_eq!(blue.focus.a, 0.5);
    let yellow = tone::adwaita(rgb([0xf6, 0xd3, 0x2d]), white);
    assert!(yellow.on_fill.luminance() < 0.1, "dark text on a light accent");
    assert!(contrast(yellow.on_fill, yellow.fill) >= 4.5);
    let dark = tone::adwaita(rgb([0x35, 0x84, 0xe4]), rgb([0x1d, 0x1d, 0x20]));
    assert!(contrast(dark.text, rgb([0x1d, 0x1d, 0x20])) >= 4.5);
}

#[test]
fn a_new_accent_builds_a_new_kit_and_the_old_one_stays_cached() {
    let a = tone::mac(rgb([0x00, 0x7a, 0xff]), rgb([0x00, 0x7a, 0xff]).alpha(0.5));
    let b = tone::mac(rgb([0xa5, 0x50, 0xa7]), rgb([0xa5, 0x50, 0xa7]).alpha(0.5));
    let first = cached(Look::MacOs, false, a);
    assert!(Arc::ptr_eq(&first, &cached(Look::MacOs, false, a)), "same accent, same kit");
    let purple = cached(Look::MacOs, false, b);
    assert!(!Arc::ptr_eq(&first, &purple));
    assert_eq!(purple.c.bg_accent, b.fill);
    assert_eq!(purple.c.border_focus, b.focus);
    assert_ne!(key(Look::MacOs, false, &a), key(Look::MacOs, false, &b), "row caches see the change");
    assert!(Arc::ptr_eq(&first, &cached(Look::MacOs, false, a)), "switching back reuses the kit");
    // The light off-switch fix stays on macOS whatever the accent.
    assert_ne!(purple.c.layer_04, Color::WHITE);
}

fn oklab_distance(a: Color, b: Color) -> f32 {
    let (a, b) = (a.to_oklab(), b.to_oklab());
    (0..3).map(|i| (a[i] - b[i]).powi(2)).sum::<f32>().sqrt()
}

/// The main text and surface pairs of every look in both appearances.
#[test]
fn every_look_reads_at_wcag_aa() {
    for look in LOOKS {
        for dark in [false, true] {
            let k = build(look, dark, tone(look, dark));
            let p = paint_for(look, &k);
            let c = &k.c;
            let name = format!("{look:?} {}", if dark { "dark" } else { "light" });
            for (bg_name, bg) in [("view", c.bg_base), ("window", c.bg_deep)] {
                let check = |what: &str, fg: Color, min: f32| {
                    let r = contrast(fg, bg);
                    assert!(r >= min, "{name}: {what} on {bg_name} is {r:.2}:1 ({} on {})", fg.hex(), bg.hex());
                };
                check("text", c.text_base, 7.0);
                check("muted text", c.text_muted, 4.5);
                check("faint text", c.text_faint, 3.0);
                check("danger", c.danger.fg, 4.5);
                check("success", c.success.fg, 4.5);
                check("warning", c.warning.fg, 4.5);
                check("accent icons", c.text_accent, 3.0);
                check("folder icons", p.folder, if look == Look::Windows { 1.4 } else { 3.0 });
            }
            let on = |fg: Color, surface: Color| contrast(fg, flatten(surface, c.bg_base));
            // Buttons: neutral ones carry body text; the default button is the accent with its
            // ink, which on macOS (white on #007AFF, 4.0:1) and GNOME (white on #3584E4, 3.9:1)
            // is natively below 4.5 but above the 3:1 UI-component floor.
            assert!(on(c.text_base, c.bg_button_neutral) >= 4.5, "{name}: neutral button text");
            let primary = super::primary(&k);
            let r = contrast(primary.c.text_contrast, primary.c.bg_contrast);
            assert!(r >= 3.0, "{name}: primary button text is {r:.2}:1");
            assert!(primary.c.bg_contrast == c.bg_accent, "{name}: primary buttons are accent-filled");
            // Selections: visible on the list, and their text readable.
            for (what, sel) in [("focused", p.selected), ("unfocused", p.selected_inactive)] {
                let fill = flatten(sel, c.bg_base);
                let r = contrast(fill, c.bg_base);
                assert!(r >= 1.15, "{name}: {what} selection is invisible ({r:.2}:1)");
                let ink = if what == "focused" { p.selected_ink.unwrap_or(c.text_base) } else { c.text_base };
                assert!(contrast(ink, fill) >= 4.5, "{name}: text {} on the {what} selection {} ({})", ink.hex(), fill.hex(), sel.hex());
            }
            assert!(contrast(flatten(p.sidebar_selected, c.bg_deep), c.bg_deep) >= 1.1, "{name}: sidebar selection");
            // Off switches: the track shows on the surface and the knob on the track.
            let track = flatten(c.layer_04, c.bg_base);
            assert!(contrast(track, c.bg_base) >= 1.2, "{name}: off track {} on {}", track.hex(), c.bg_base.hex());
            assert!(contrast(c.knob, track) >= 1.2, "{name}: knob on off track");
        }
    }
}

#[test]
fn non_mac_looks_set_their_platform_metrics() {
    let fluent = build(Look::Windows, false, tone(Look::Windows, false));
    assert_eq!((fluent.radius.md, fluent.radius.lg), (4.0, 8.0));
    assert_eq!(fluent.ty.ui.size, 14.0);
    let adw = build(Look::Gnome, true, tone(Look::Gnome, true));
    assert_eq!((adw.radius.md, adw.radius.lg), (6.0, 12.0));
    assert_eq!(adw.c.bg_deep.hex(), "#222226ff");
    assert_eq!(adw.c.bg_accent.hex(), "#3584e4ff");
    assert_eq!(Look::Windows.row_height(), 32.0);
    assert_eq!(Look::Gnome.row_height(), 34.0);
    assert!(Look::MacOs.families().is_none(), "macOS keeps Rune's system font");
}

#[test]
fn appearance_flag_forces_light_or_dark() {
    assert_eq!(appearance_arg(args(&["--look", "windows", "--appearance", "dark"])), Some(true));
    assert_eq!(appearance_arg(args(&["--appearance", "light"])), Some(false));
    assert_eq!(appearance_arg(args(&["--appearance", "sepia"])), None);
    assert_eq!(appearance_arg(args(&["--theme", "dark"])), None);
}
