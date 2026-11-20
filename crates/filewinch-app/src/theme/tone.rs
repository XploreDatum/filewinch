//! The accent each look paints with, resolved for one appearance from what the platform reports
//! (pure functions, so the fallback rules are testable on any host), and the colour arithmetic
//! the looks share: WCAG contrast, sRGB compositing and lightness steps.

use filewinch_platform_look::{Accent, Rgb};
use rune::Color;
use rune::color::{srgb_decode, srgb_encode};

/// A look's accent for one appearance.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tone {
    /// Primary buttons, on switches, selection pills: macOS's default button, Fluent's
    /// `AccentFillColorDefault`, Adwaita's `accent_bg_color`.
    pub fill: Color,
    /// Text and icons on `fill`.
    pub on_fill: Color,
    /// The accent as text or an icon on the window (links, folder icons, the title glyph).
    pub text: Color,
    /// Keyboard focus rings.
    pub focus: Color,
}

/// sRGB bytes as a (linear) Rune colour.
pub fn rgb([r, g, b]: Rgb) -> Color {
    let d = |v: u8| srgb_decode(v as f32 / 255.0);
    Color { r: d(r), g: d(g), b: d(b), a: 1.0 }
}

/// `c` composited over the opaque `bg` as windows blend by default (in sRGB).
pub fn flatten(c: Color, bg: Color) -> Color {
    let m = |x: f32, y: f32| srgb_decode(srgb_encode(x) * c.a + srgb_encode(y) * (1.0 - c.a));
    Color { r: m(c.r, bg.r), g: m(c.g, bg.g), b: m(c.b, bg.b), a: 1.0 }
}

/// WCAG 2 contrast ratio of `fg` (composited over `bg` if translucent) against opaque `bg`.
pub fn contrast(fg: Color, bg: Color) -> f32 {
    let (a, b) = (flatten(fg, bg).luminance(), bg.luminance());
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// `c` with its OKLab lightness moved by `dl`, hue kept.
pub fn lighten(c: Color, dl: f32) -> Color {
    let [l, a, b, al] = c.to_oklab();
    Color::from_oklab([(l + dl).clamp(0.0, 1.0), a, b, al])
}

/// `c`, darkened (on a light `bg`) or lightened (on a dark one) in small lightness steps until it
/// reaches `min` contrast against `bg`; `c` itself when it already does.
pub fn readable(c: Color, bg: Color, min: f32) -> Color {
    let step = if bg.luminance() > 0.18 { -0.01 } else { 0.01 };
    let mut out = c;
    for _ in 0..100 {
        if contrast(out, bg) >= min {
            break;
        }
        out = lighten(out, step);
    }
    out
}

/// White, or near-black when white would read worse than 3:1 on `fill` (a light accent).
fn ink_on(fill: Color) -> Color {
    if contrast(Color::WHITE, fill) >= 3.0 { Color::WHITE } else { rune::oklch(0.18, 0.0, 0.0) }
}

/// macOS: the system accent and focus ring as Rune reads them (`Cx::accent`, `focus_color`).
/// AppKit's default button is the accent with white text.
pub fn mac(accent: Color, focus: Color) -> Tone {
    Tone { fill: accent, on_fill: Color::WHITE, text: accent, focus }
}

/// Windows 11: accent fills are `AccentDark1` in light mode and `AccentLight2` in dark mode,
/// with `TextOnAccentFillColorPrimary` (white, or black on the light dark-mode fill). Shades the
/// platform didn't give are derived from the base, as far from it as Windows' own default ramp
/// (#0078D4 → #005FB8 / #60CDFF). Focus rings are Fluent's neutral outer stroke, the text colour.
pub fn fluent(a: Accent, dark: bool, bg: Color, fg: Color) -> Tone {
    let base = rgb(a.base);
    let fill = if dark {
        a.light2.map(rgb).unwrap_or_else(|| lighten(base, 0.23))
    } else {
        a.dark1.map(rgb).unwrap_or_else(|| lighten(base, -0.08))
    };
    Tone { fill, on_fill: if dark { ink_on_dark(fill) } else { ink_on(fill) }, text: readable(fill, bg, 4.5), focus: fg }
}

/// Fluent's dark-mode accent text is black on the light fill unless the fill is dark.
fn ink_on_dark(fill: Color) -> Color {
    if contrast(Color::BLACK, fill) >= contrast(Color::WHITE, fill) { Color::BLACK } else { Color::WHITE }
}

/// GNOME (libadwaita) and KDE: `accent_bg_color` is the accent itself with white text (dark
/// text on a light accent), `accent_color` (text on the window) is the accent stepped until it
/// reads at 4.5:1, as libadwaita derives it, and focus rings are the accent at 50%.
pub fn adwaita(base: Color, bg: Color) -> Tone {
    Tone { fill: base, on_fill: ink_on(base), text: readable(base, bg, 4.5), focus: base.alpha(0.5) }
}
