//! One `Kit` per look, built from that platform's published design tokens: Rune's macOS kit,
//! Windows 11 Fluent (WinUI 3 theme resources) and GNOME libadwaita, with KDE Breeze colours.
//! Only tokens change here; rune-kit's control geometry stays AppKit's (see
//! `docs/rune-issues/3-native-kits-windows-linux.md`).

use rune::{Color, Family, Shadow, TextStyle, color::hex, oklch};
use rune_kit::{Elevation, Kit, PaletteFocus, PaletteSeed, Radius, StateColors, Type};

use super::tone::{Tone, flatten, readable};

fn h(s: &str) -> Color {
    hex(s).unwrap_or(Color::BLACK)
}

fn ring(spread: f32, color: Color) -> Shadow {
    Shadow { spread, color, ..Shadow::default() }
}

fn drop(y: f32, blur: f32, color: Color) -> Shadow {
    Shadow { y, blur, color, ..Shadow::default() }
}

/// A 1-point line along the inside bottom edge (Fluent's control and text-box elevation border).
fn bottom_edge(color: Color) -> Shadow {
    Shadow { y: -1.0, color, inset: true, ..Shadow::default() }
}

/// Elevation layers, in `Elevation`'s field order (its `input` is private, so no struct literal).
fn elev(raised: Vec<Shadow>, floating: Vec<Shadow>, overlay: Vec<Shadow>, button_neutral: Vec<Shadow>,
    button_contrast: Vec<Shadow>, md: Vec<Shadow>, knob: Vec<Shadow>) -> Elevation {
    let mut e = Elevation::default();
    (e.raised, e.floating, e.overlay, e.button_neutral, e.button_contrast, e.md, e.knob) =
        (raised, floating, overlay, button_neutral, button_contrast, md, knob);
    e
}

/// The accent tokens every look shares.
fn apply_tone(k: &mut Kit, t: Tone) {
    let c = &mut k.c;
    c.bg_accent = t.fill;
    c.text_accent = t.text;
    c.icon_accent = t.text;
    c.border_focus = t.focus;
    c.border_active = t.fill;
    c.knob_on = if t.on_fill.luminance() < 0.5 { t.on_fill } else { c.knob_on };
}

/// macOS: Rune's native kit, which follows AppKit's palette and metrics, with the system accent.
pub fn mac(dark: bool, t: Tone) -> Kit {
    let mut k = Kit::neutral(dark);
    if !dark {
        // Rune's light `tok::indicator` (the switch's off track, `layer_04`) is pure white, the
        // same as the knob, so off switches vanish. AppKit's light off-track grey until Rune
        // fixes it (docs/rune-issues/1-neutral-off-switch.md). Only the switch reads `layer_04`.
        k.c.layer_04 = oklch(0.925, 0.003, 265.0);
    }
    apply_tone(&mut k, t);
    k
}

/// A status role: `fg` as text (stepped to 4.5:1 on both surfaces if needed), a quiet `bg` wash.
fn state(fg: Color, bg: Color, surfaces: [Color; 2]) -> StateColors {
    let fg = surfaces.iter().fold(fg, |c, s| readable(c, *s, 4.5));
    StateColors { bg, fg, border: fg.alpha(0.35), icon: fg }
}

/// Surfaces and inks a non-macOS look sets on top of `Kit::from_palette`'s derived tokens.
struct Surfaces {
    window: Color,
    view: Color,
    control: Color,
    text: Color,
    muted: Option<Color>,
    faint: Option<Color>,
    stroke: Color,
    divider: Color,
    off_track: Color,
}

fn base_kit(dark: bool, s: &Surfaces, t: Tone) -> Kit {
    let mut k = Kit::from_palette_focus(PaletteSeed { bg: s.view, fg: s.text, accent: t.fill }, PaletteFocus::Custom(t.focus));
    debug_assert_eq!(k.dark, dark);
    // Our rings carry their own colours.
    k.ring = None;
    let c = &mut k.c;
    c.page = s.window;
    c.bg_deep = s.window;
    c.layer_01 = s.window;
    c.layer_04 = s.off_track;
    c.bg_base = s.view;
    c.surface_raised = s.view;
    c.surface_raised_base = s.view;
    c.bg_button_neutral = s.control;
    // Secondary text keeps its floors (4.5:1 muted, 3:1 faint) on the window as well as the view
    // `from_palette` measured it against.
    let floor = |c: Color, min: f32| readable(readable(c, s.window, min), s.view, min);
    let m = floor(s.muted.unwrap_or(c.text_muted), 4.5);
    (c.text_muted, c.text_weak, c.icon_muted, c.std_icon) = (m, m, m, m);
    let f = floor(s.faint.unwrap_or(c.text_faint), 3.0);
    (c.text_faint, c.icon_faint, c.icon_weaker) = (f, f, f);
    c.border_base = s.stroke;
    c.border_muted = s.divider;
    c.border_weak = s.divider;
    c.knob = Color::WHITE;
    c.knob_on = Color::WHITE;
    apply_tone(&mut k, t);
    k
}

fn ty(family_mono: f32, ramp: [(f32, f32); 3], regular: f32, medium: f32, strong: f32) -> Type {
    // ramp: body (size, line), caption, compact line height for body-sized controls.
    let [(body, body_lh), (caption, caption_lh), (_, compact_lh)] = ramp;
    let s = |size: f32, w: f32, lh: f32| TextStyle::new(size, w).lh(lh);
    Type {
        ui: s(body, regular, body_lh),
        ui_medium: s(body, strong, body_lh),
        ui_compact: s(body, regular, compact_lh),
        small: s(caption, regular, caption_lh),
        small_medium: s(caption, strong, caption_lh),
        body: s(body, regular, body_lh),
        body_medium: s(body, strong, body_lh),
        text_small: s(body, regular, body_lh),
        text_small_medium: s(body, medium, body_lh),
        text_xsmall: s(caption, regular, caption_lh),
        text_xsmall_medium: s(caption, strong, caption_lh),
        mono: s(family_mono, regular, compact_lh.max(family_mono + 4.0)).family(Family::SystemMono),
    }
}

/// Windows 11 Fluent: Mica window (`SolidBackgroundFillColorBase`), layer and card fills,
/// control strokes with the darker bottom edge, Segoe UI Variable's type ramp (Body 14/20,
/// Caption 12/16, Semibold for strong text), radii 4 (controls) and 8 (overlays), Fluent's
/// flyout and dialog shadows, and its status colours.
pub fn fluent(dark: bool, t: Tone) -> Kit {
    let s = if dark {
        Surfaces {
            window: h("#202020"),
            view: h("#2b2b2b"),
            control: h("#2d2d2d"),
            text: h("#ffffff"),
            muted: Some(h("#c5c5c5")),
            faint: Some(h("#9a9a9a")),
            stroke: Color::WHITE.alpha(0.07),
            divider: Color::WHITE.alpha(0.08),
            off_track: h("#5d5d5d"),
        }
    } else {
        Surfaces {
            window: h("#f3f3f3"),
            view: h("#fbfbfb"),
            control: h("#fbfbfb"),
            text: h("#1b1b1b"),
            muted: Some(h("#5f5f5f")),
            faint: Some(h("#8a8a8a")),
            stroke: Color::BLACK.alpha(0.06),
            divider: Color::BLACK.alpha(0.08),
            off_track: h("#c4c4c4"),
        }
    };
    let mut k = base_kit(dark, &s, t);
    let surfaces = [s.view, s.window];
    let c = &mut k.c;
    if dark {
        c.danger = state(h("#ff99a4"), h("#442726"), surfaces);
        c.success = state(h("#6ccb5f"), h("#393d1b"), surfaces);
        c.warning = state(h("#fce100"), h("#433519"), surfaces);
    } else {
        c.danger = state(h("#c42b1c"), h("#fde7e9"), surfaces);
        c.success = state(h("#0f7b0f"), h("#dff6dd"), surfaces);
        c.warning = state(h("#9d5d00"), h("#fff4ce"), surfaces);
    }
    c.hover = c.text_base.alpha(if dark { 0.06 } else { 0.04 });
    c.pressed = c.text_base.alpha(if dark { 0.04 } else { 0.02 });
    c.scrim = Color::BLACK.alpha(0.3);
    k.ty = ty(13.0, [(14.0, 20.0), (12.0, 16.0), (14.0, 18.0)], 400.0, 600.0, 600.0);
    k.radius = Radius { xs: 2.0, sm: 4.0, md: 4.0, lg: 8.0, xl: 8.0 };
    let (stroke, edge, a) = if dark {
        (Color::WHITE.alpha(0.07), Color::BLACK.alpha(0.2), 0.26)
    } else {
        (Color::BLACK.alpha(0.06), Color::BLACK.alpha(0.16), 0.14)
    };
    let flyout = vec![drop(8.0, 16.0, Color::BLACK.alpha(a)), ring(1.0, stroke)];
    k.elevation = elev(
        vec![ring(1.0, stroke)],
        flyout.clone(),
        vec![drop(32.0, 64.0, Color::BLACK.alpha(a + 0.05)), drop(2.0, 21.0, Color::BLACK.alpha(a)), ring(1.0, stroke)],
        vec![ring(1.0, stroke), bottom_edge(edge)],
        vec![bottom_edge(Color::BLACK.alpha(0.2))],
        flyout,
        vec![drop(1.0, 2.0, Color::BLACK.alpha(0.2))],
    )
    .with_input([ring(1.0, stroke), bottom_edge(if dark { Color::WHITE.alpha(0.54) } else { Color::BLACK.alpha(0.45) })]);
    k
}

/// GNOME libadwaita (1.6+): window `#fafafb`/`#222226`, view `#ffffff`/`#1d1d20`, borders at
/// 15% of the text colour, flat buttons (`alpha(currentColor, .1)`), radii 6 (controls) and 12
/// (cards, dialogs), Adwaita Sans 11 pt (14.67 px) with bold headings and buttons. Dimmed text
/// is Adwaita's 55% only where that reads at 4.5:1; otherwise `Kit::from_palette`'s floor.
pub fn adwaita(dark: bool, t: Tone) -> Kit {
    let s = if dark {
        Surfaces {
            window: h("#222226"),
            view: h("#1d1d20"),
            control: Color::WHITE.alpha(0.1),
            text: h("#ffffff"),
            muted: None,
            faint: None,
            stroke: Color::WHITE.alpha(0.15),
            divider: Color::WHITE.alpha(0.1),
            off_track: h("#4a4a4f"),
        }
    } else {
        Surfaces {
            window: h("#fafafb"),
            view: h("#ffffff"),
            control: h("#000006").alpha(0.1),
            text: flatten(h("#000006").alpha(0.8), h("#ffffff")),
            muted: None,
            faint: None,
            stroke: h("#000006").alpha(0.15),
            divider: h("#000006").alpha(0.1),
            off_track: h("#d2d2d4"),
        }
    };
    let mut k = adwaita_like(dark, &s, t);
    let surfaces = [s.view, s.window];
    let c = &mut k.c;
    if dark {
        c.danger = state(h("#ff7b63"), h("#ff7b63").alpha(0.15), surfaces);
        c.success = state(h("#8ff0a4"), h("#8ff0a4").alpha(0.12), surfaces);
        c.warning = state(h("#f8e45c"), h("#f8e45c").alpha(0.12), surfaces);
    } else {
        c.danger = state(h("#c01c28"), h("#e01b24").alpha(0.1), surfaces);
        c.success = state(h("#1b8553"), h("#26a269").alpha(0.1), surfaces);
        c.warning = state(h("#9c6e03"), h("#e5a50a").alpha(0.12), surfaces);
    }
    k.ty = ty(13.33, [(14.67, 20.0), (12.0, 16.0), (14.67, 18.0)], 400.0, 600.0, 700.0);
    k.radius = Radius { xs: 3.0, sm: 5.0, md: 6.0, lg: 12.0, xl: 12.0 };
    k
}

/// KDE Plasma's Breeze colours on the Adwaita structure: window `#eff0f1`/`#202326`, view
/// `#ffffff`/`#141618`, Breeze blue `#3daee9`, Noto Sans 10 pt, radii 3 and 5.
pub fn breeze(dark: bool, t: Tone) -> Kit {
    let s = if dark {
        Surfaces {
            window: h("#202326"),
            view: h("#141618"),
            control: h("#292c30"),
            text: h("#fcfcfc"),
            muted: None,
            faint: None,
            stroke: Color::WHITE.alpha(0.15),
            divider: Color::WHITE.alpha(0.1),
            off_track: h("#4d5157"),
        }
    } else {
        Surfaces {
            window: h("#eff0f1"),
            view: h("#ffffff"),
            control: h("#fcfcfc"),
            text: h("#232629"),
            muted: None,
            faint: None,
            stroke: h("#232629").alpha(0.2),
            divider: h("#232629").alpha(0.12),
            off_track: h("#c9cdd1"),
        }
    };
    let mut k = adwaita_like(dark, &s, t);
    let surfaces = [s.view, s.window];
    let c = &mut k.c;
    c.danger = state(h("#da4453"), h("#da4453").alpha(0.12), surfaces);
    c.success = state(h("#27ae60"), h("#27ae60").alpha(0.12), surfaces);
    c.warning = state(h("#f67400"), h("#f67400").alpha(0.12), surfaces);
    k.ty = ty(13.33, [(13.33, 19.0), (10.67, 15.0), (13.33, 17.0)], 400.0, 600.0, 600.0);
    k.radius = Radius { xs: 2.0, sm: 3.0, md: 3.0, lg: 5.0, xl: 5.0 };
    if !k.dark {
        // Breeze buttons are outlined, not flat.
        k.elevation.button_neutral = vec![ring(1.0, s.stroke)];
    }
    k
}

/// Flat controls and soft card and dialog shadows, as libadwaita draws them.
fn adwaita_like(dark: bool, s: &Surfaces, t: Tone) -> Kit {
    let mut k = base_kit(dark, s, t);
    let a = if dark { 0.36 } else { 0.09 };
    let line = if dark { Color::BLACK.alpha(0.5) } else { Color::BLACK.alpha(0.06) };
    let popover = vec![drop(1.0, 5.0, Color::BLACK.alpha(a)), drop(2.0, 14.0, Color::BLACK.alpha(a * 0.6)), ring(1.0, line)];
    k.elevation = elev(
        vec![ring(1.0, line), drop(1.0, 3.0, Color::BLACK.alpha(a * 0.8))],
        popover.clone(),
        popover.clone(),
        vec![],
        vec![],
        popover,
        vec![drop(1.0, 2.0, Color::BLACK.alpha(0.2)), ring(0.5, Color::BLACK.alpha(0.1))],
    )
    .with_input([ring(1.0, s.stroke)]);
    let c = &mut k.c;
    c.hover = c.text_base.alpha(0.07);
    c.pressed = c.text_base.alpha(0.16);
    k
}
