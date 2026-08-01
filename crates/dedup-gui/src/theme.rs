//! LCARS-inspired theme: black backdrop, warm amber text, and rounded coloured
//! "pill" widgets in the classic orange / lilac / periwinkle palette.
//!
//! The colours are a [`Palette`] value rather than constants, installed per
//! thread, so a second appearance is expressible. Read them through the
//! accessors ([`text`], [`red`], …) — the raw values are private precisely so
//! that new code cannot bypass the active palette.

use egui::style::WidgetVisuals;
use egui::{Color32, CornerRadius, Stroke, Style, Visuals};

const BLACK: Color32 = Color32::from_rgb(0x00, 0x00, 0x00);
const ORANGE: Color32 = Color32::from_rgb(0xFF, 0x99, 0x00);
const AMBER: Color32 = Color32::from_rgb(0xFF, 0xCC, 0x66);
const TAN: Color32 = Color32::from_rgb(0xFF, 0xCC, 0x99);
const LILAC: Color32 = Color32::from_rgb(0xCC, 0x99, 0xFF);
const BLUE: Color32 = Color32::from_rgb(0x66, 0x99, 0xFF);
const RED: Color32 = Color32::from_rgb(0xE0, 0x66, 0x55);
/// Muted LCARS green for "added" review rows (kept distinct from the amber/red
/// pills so the review table's colour cue reads clearly on the black backdrop).
const GREEN: Color32 = Color32::from_rgb(0x66, 0xCC, 0x77);
/// Dim warm grey for "unchanged" review rows — present but visually recessive.
const GREY: Color32 = Color32::from_rgb(0x8A, 0x82, 0x74);
const TEXT: Color32 = Color32::from_rgb(0xEB, 0xD0, 0xA0);
const PANEL: Color32 = Color32::from_rgb(0x0A, 0x08, 0x0C);
/// Subtle warm outline that separates dark image content from the dark panels.
const HAIRLINE: Color32 = Color32::from_rgb(0x5C, 0x4E, 0x40);

/// The colours one appearance draws with.
///
/// A value rather than a set of constants, so a second appearance is
/// expressible at all. Fields are added as accessors are driven out by tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub black: Color32,
    pub orange: Color32,
    pub amber: Color32,
    pub tan: Color32,
    pub lilac: Color32,
    pub blue: Color32,
    pub red: Color32,
    pub green: Color32,
    pub grey: Color32,
    pub text: Color32,
    pub panel: Color32,
    pub hairline: Color32,
}

/// The appearance the application has always had.
pub const DARK: Palette = Palette {
    black: BLACK,
    orange: ORANGE,
    amber: AMBER,
    tan: TAN,
    lilac: LILAC,
    blue: BLUE,
    red: RED,
    green: GREEN,
    grey: GREY,
    text: TEXT,
    panel: PANEL,
    hairline: HAIRLINE,
};

thread_local! {
    /// The palette the current thread draws with.
    ///
    /// Thread-local rather than a process-wide global on purpose: the test
    /// suite runs in parallel, and a shared palette would let a test asserting
    /// one appearance race a test asserting another.
    static ACTIVE: std::cell::Cell<Palette> = const { std::cell::Cell::new(DARK) };
}

/// Make `palette` the one this thread draws with, from the next read onwards.
///
/// Nothing caches a colour, so there is nothing to invalidate — the next frame
/// paints with the new appearance.
pub fn install(palette: Palette) {
    ACTIVE.with(|p| p.set(palette));
}

/// Read one colour from the thread's active palette.
macro_rules! colour {
    ($($name:ident),+ $(,)?) => {
        $(pub fn $name() -> Color32 {
            ACTIVE.with(|p| p.get().$name)
        })+
    };
}

colour!(
    black, orange, amber, tan, lilac, blue, red, green, grey, text, panel, hairline
);

/// Large corner radius gives widgets the rounded LCARS block look.
pub const PILL: CornerRadius = CornerRadius::same(12);

/// FNV-1a hash of a string, used to derive stable per-name colors/patterns.
pub fn name_hash(s: &str) -> u32 {
    let mut hash: u32 = 2166136261;
    for b in s.bytes() {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(16777619);
    }
    hash
}

/// HSL → sRGB. Hue in degrees [0,360), saturation/lightness in [0,1].
pub fn hsl(h: f32, s: f32, l: f32) -> Color32 {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to = |v: f32| (((v + m) * 255.0).round()).clamp(0.0, 255.0) as u8;
    Color32::from_rgb(to(r), to(g), to(b))
}

/// A warm outline used on every button so an unselected (panel-filled) pill still
/// reads as clickable — the LCARS look leans on outlined bars.
const BUTTON_EDGE: Color32 = Color32::from_rgb(0x8A, 0x72, 0x4E);

fn pill(bg: Color32, fg: Color32) -> WidgetVisuals {
    WidgetVisuals {
        bg_fill: bg,
        weak_bg_fill: bg,
        bg_stroke: Stroke::new(1.0, BUTTON_EDGE),
        corner_radius: PILL,
        fg_stroke: Stroke::new(1.5, fg),
        expansion: 0.0,
    }
}

/// Install `palette` on this thread and build the matching egui style on `ctx`.
///
/// Both halves happen here so egui's own chrome and the colours the application
/// paints can never disagree about which appearance is active.
pub fn apply(ctx: &egui::Context, palette: Palette) {
    install(palette);
    let p = palette;
    let mut style = Style::default();
    let mut v = Visuals::dark();

    v.dark_mode = true;
    v.window_fill = p.black;
    v.panel_fill = p.black;
    v.faint_bg_color = p.panel;
    v.extreme_bg_color = Color32::from_rgb(0x12, 0x0D, 0x16);
    v.override_text_color = Some(p.text);
    v.hyperlink_color = p.blue;
    v.window_stroke = Stroke::new(1.5, p.orange);
    v.selection.bg_fill = Color32::from_rgb(0x24, 0x33, 0x5C);
    v.selection.stroke = Stroke::new(1.0, p.blue);

    // Buttons: orange at rest, amber on hover, tan when pressed, lilac when open.
    v.widgets.inactive = pill(p.orange, p.black);
    v.widgets.hovered = pill(p.amber, p.black);
    v.widgets.active = pill(p.tan, p.black);
    v.widgets.open = pill(p.lilac, p.black);
    v.widgets.noninteractive.corner_radius = PILL;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x3A, 0x2A, 0x1E));
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);

    style.visuals = v;
    style.spacing.button_padding = egui::vec2(12.0, 6.0);
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    // Solid (non-floating) scrollbars: overflowing lists get a permanent bar
    // instead of a hover-only overlay, so long result pages are visibly
    // scrollable.
    style.spacing.scroll = egui::style::ScrollStyle::solid();
    ctx.all_styles_mut(move |s| *s = style.clone());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Characterization: the dark palette's values are what the application has
    /// always drawn with. Pinned before the refactor so converting the palette to
    /// data cannot shift the appearance by accident.
    /// Applying the theme is what makes a palette active, so the style egui
    /// draws chrome with and the colours the application draws with can never
    /// disagree about the appearance.
    #[test]
    fn applying_the_theme_installs_the_palette_it_was_given() {
        let marker = Color32::from_rgb(0x01, 0x02, 0x03);
        let ctx = egui::Context::default();

        apply(
            &ctx,
            Palette {
                text: marker,
                ..DARK
            },
        );
        assert_eq!(text(), marker);
        assert_eq!(
            ctx.global_style().visuals.override_text_color,
            Some(marker),
            "and egui's own chrome uses it too"
        );

        apply(&ctx, DARK);
        assert_eq!(text(), DARK.text);
    }

    /// Each thread has its own palette. This is what lets the suite keep running
    /// tests in parallel once there are two appearances to assert: with a shared
    /// global, a test asserting one would race a test asserting the other, and
    /// the failures would look like colour or layout bugs rather than a race.
    #[test]
    fn a_palette_installed_on_one_thread_is_not_seen_by_another() {
        let marker = Color32::from_rgb(0xAB, 0xCD, 0xEF);
        install(Palette {
            text: marker,
            ..DARK
        });
        assert_eq!(text(), marker);

        let seen_elsewhere = std::thread::spawn(text).join().expect("thread");
        assert_eq!(
            seen_elsewhere, DARK.text,
            "another thread still draws with its own palette"
        );
        assert_eq!(text(), marker, "and this thread keeps the one it installed");
    }

    /// A palette is data: installing one is what makes a second appearance
    /// reachable, and it takes effect immediately with nothing to invalidate.
    #[test]
    fn installing_a_palette_changes_what_the_accessors_read() {
        let other = Palette {
            text: Color32::from_rgb(0x11, 0x22, 0x33),
            ..DARK
        };
        install(other);
        assert_eq!(text(), Color32::from_rgb(0x11, 0x22, 0x33));
        // The colours it did not change still read from the installed palette.
        assert_eq!(red(), DARK.red);

        install(DARK);
        assert_eq!(text(), DARK.text);
    }

    #[test]
    fn every_accessor_reads_the_dark_palettes_value() {
        let rgb = Color32::from_rgb;
        assert_eq!(black(), rgb(0x00, 0x00, 0x00));
        assert_eq!(orange(), rgb(0xFF, 0x99, 0x00));
        assert_eq!(amber(), rgb(0xFF, 0xCC, 0x66));
        assert_eq!(tan(), rgb(0xFF, 0xCC, 0x99));
        assert_eq!(lilac(), rgb(0xCC, 0x99, 0xFF));
        assert_eq!(blue(), rgb(0x66, 0x99, 0xFF));
        assert_eq!(red(), rgb(0xE0, 0x66, 0x55));
        assert_eq!(green(), rgb(0x66, 0xCC, 0x77));
        assert_eq!(grey(), rgb(0x8A, 0x82, 0x74));
        assert_eq!(text(), rgb(0xEB, 0xD0, 0xA0));
        assert_eq!(panel(), rgb(0x0A, 0x08, 0x0C));
        assert_eq!(hairline(), rgb(0x5C, 0x4E, 0x40));
    }
}
