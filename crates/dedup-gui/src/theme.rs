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
    /// Whether this is a dark appearance — selects egui's base `Visuals` and
    /// the `dark_mode` flag its own chrome reads.
    pub dark_mode: bool,
    /// The darkest ink: text drawn *on* a coloured pill, and other high-contrast
    /// marks. Stays dark in both appearances (black on an orange pill reads on
    /// either background) — it is **not** the window backdrop; that is `bg`.
    pub black: Color32,
    /// The window/panel backdrop. Black in dark, near-white in light.
    pub bg: Color32,
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
    dark_mode: true,
    black: BLACK,
    bg: BLACK,
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

/// The light appearance. Hand-derived, not an inversion of dark: the five
/// review-board semantics (grey unchanged, green only-here, red will-delete,
/// amber differs, blue resurrection) are dark variants chosen to stay pairwise
/// distinct on a light background, where a mechanical flip would collapse "will
/// delete" into "differs". Starting values — the user is the customer and tunes
/// them by eye against the both-palettes image.
pub const LIGHT: Palette = Palette {
    dark_mode: false,
    black: Color32::from_rgb(0x1A, 0x14, 0x10),
    bg: Color32::from_rgb(0xF2, 0xEE, 0xE6),
    orange: Color32::from_rgb(0xD9, 0x7A, 0x00),
    amber: Color32::from_rgb(0xB5, 0x82, 0x0E),
    tan: Color32::from_rgb(0xC7, 0x9A, 0x5E),
    lilac: Color32::from_rgb(0x82, 0x57, 0xC7),
    blue: Color32::from_rgb(0x2C, 0x6F, 0xB5),
    red: Color32::from_rgb(0xB0, 0x36, 0x2A),
    green: Color32::from_rgb(0x2E, 0x7D, 0x4F),
    grey: Color32::from_rgb(0x8C, 0x87, 0x7C),
    text: Color32::from_rgb(0x2A, 0x20, 0x18),
    panel: Color32::from_rgb(0xE3, 0xDD, 0xD1),
    hairline: Color32::from_rgb(0xB8, 0xAE, 0x9C),
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
    black, bg, orange, amber, tan, lilac, blue, red, green, grey, text, panel, hairline
);

/// Whether the active palette is a dark appearance. Lets a few pieces that
/// aren't a single flat colour (the identicon's tile and cell lightness) adapt
/// without threading the whole palette through.
pub fn is_dark() -> bool {
    ACTIVE.with(|p| p.get().dark_mode)
}

/// Perceptual colour distance (the "redmean" approximation of ΔE). Two colours
/// that differ by one channel value are numerically unequal but visually the
/// same; this measures whether they are *distinguishable*. Range ≈ 0–765.
///
/// A test-only assertion helper: the palettes are static data, so their
/// separation is proven once in the suite rather than recomputed at runtime.
#[cfg(test)]
pub fn perceptual_distance(a: Color32, b: Color32) -> f32 {
    let (r1, g1, b1) = (a.r() as f32, a.g() as f32, a.b() as f32);
    let (r2, g2, b2) = (b.r() as f32, b.g() as f32, b.b() as f32);
    let rbar = (r1 + r2) / 2.0;
    let (dr, dg, db) = (r1 - r2, g1 - g2, b1 - b2);
    ((2.0 + rbar / 256.0) * dr * dr + 4.0 * dg * dg + (2.0 + (255.0 - rbar) / 256.0) * db * db)
        .sqrt()
}

/// WCAG relative luminance of a colour (0 = black, 1 = white).
fn relative_luminance(c: Color32) -> f32 {
    let f = |v: u8| {
        let s = v as f32 / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
}

/// WCAG contrast ratio between two colours (1 = identical, 21 = black/white).
pub fn contrast_ratio(a: Color32, b: Color32) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// A light ink for text on a *dark* accent fill. Warm near-white so it sits in
/// the LCARS family rather than reading as pure white.
const INK_LIGHT: Color32 = Color32::from_rgb(0xF5, 0xF1, 0xE8);

/// The legible text colour for a filled pill on `palette`, given its `fill`:
/// the palette's dark ink or a light ink, whichever contrasts more with the fill.
fn ink_on_palette(palette: &Palette, fill: Color32) -> Color32 {
    if contrast_ratio(palette.black, fill) >= contrast_ratio(INK_LIGHT, fill) {
        palette.black
    } else {
        INK_LIGHT
    }
}

/// The legible text colour for a filled pill, given its `fill`: the active
/// palette's dark ink or a light ink, whichever contrasts more with the fill.
///
/// This is why filled pills read on **both** appearances: on the dark palette
/// the accents are bright, so dark ink always wins and every pill is unchanged;
/// on the light palette the darker accents (red, blue, green, lilac) flip to the
/// light ink while the lighter ones (amber, tan) keep dark ink. Route every
/// filled pill's text through here instead of hard-coding [`black`].
pub fn ink_on(fill: Color32) -> Color32 {
    ACTIVE.with(|p| ink_on_palette(&p.get(), fill))
}

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

/// Build the egui [`Style`] for one palette (its own chrome colours, widgets and
/// spacing). Pure — installs nothing.
fn style_for(palette: Palette) -> Style {
    let p = palette;
    let mut style = Style::default();
    let mut v = if p.dark_mode {
        Visuals::dark()
    } else {
        Visuals::light()
    };

    v.dark_mode = p.dark_mode;
    v.window_fill = p.bg;
    v.panel_fill = p.bg;
    v.faint_bg_color = p.panel;
    // A slightly deeper inset than the panel, in the palette's own direction.
    v.extreme_bg_color = if p.dark_mode {
        Color32::from_rgb(0x12, 0x0D, 0x16)
    } else {
        Color32::from_rgb(0xD8, 0xD1, 0xC3)
    };
    v.override_text_color = Some(p.text);
    v.hyperlink_color = p.blue;
    v.window_stroke = Stroke::new(1.5, p.orange);
    // Selection fill: a muted tint of the accent blue, toward the backdrop.
    v.selection.bg_fill = if p.dark_mode {
        Color32::from_rgb(0x24, 0x33, 0x5C)
    } else {
        Color32::from_rgb(0xC6, 0xD8, 0xF0)
    };
    v.selection.stroke = Stroke::new(1.0, p.blue);

    // Buttons: orange at rest, amber on hover, tan when pressed, lilac when open.
    // Ink follows the fill so it stays legible on the light palette's darker
    // accents (lilac in particular), and is unchanged on dark.
    v.widgets.inactive = pill(p.orange, ink_on_palette(&p, p.orange));
    v.widgets.hovered = pill(p.amber, ink_on_palette(&p, p.amber));
    v.widgets.active = pill(p.tan, ink_on_palette(&p, p.tan));
    v.widgets.open = pill(p.lilac, ink_on_palette(&p, p.lilac));
    v.widgets.noninteractive.corner_radius = PILL;
    // Keep the dark appearance's exact prior outline; light uses its hairline.
    v.widgets.noninteractive.bg_stroke = Stroke::new(
        1.0,
        if p.dark_mode {
            Color32::from_rgb(0x3A, 0x2A, 0x1E)
        } else {
            p.hairline
        },
    );
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);

    style.visuals = v;
    // Labels never sense clicks for *text* selection. egui's default makes
    // every label a click-and-drag target, and in a list or table that label
    // sits on top of its row: the row's own click is swallowed everywhere a
    // cell has text, so a file could only be picked by hitting a gap (an
    // empty column). Selecting a row beats selecting a word here; the panes
    // where reading text is the point opt back in per label.
    style.interaction.selectable_labels = false;
    style.spacing.button_padding = egui::vec2(12.0, 6.0);
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    // Solid (non-floating) scrollbars: overflowing lists get a permanent bar
    // instead of a hover-only overlay, so long result pages are visibly
    // scrollable.
    style.spacing.scroll = egui::style::ScrollStyle::solid();
    style
}

/// Install `palette` on this thread and force it as the style everywhere on
/// `ctx`. The low-level path — used by tests and screenshots that need one
/// specific appearance regardless of the theme preference. Production drives the
/// appearance through [`register_themes`] + [`sync_active`] instead.
#[cfg(test)]
pub fn apply(ctx: &egui::Context, palette: Palette) {
    install(palette);
    let style = style_for(palette);
    ctx.all_styles_mut(move |s| *s = style.clone());
}

/// The palette for an egui theme.
fn palette_for(theme: egui::Theme) -> Palette {
    match theme {
        egui::Theme::Dark => DARK,
        egui::Theme::Light => LIGHT,
    }
}

/// Register a style for **each** theme, so egui draws its own chrome with the
/// one matching the resolved preference. Called once; the live choice is then
/// egui's to make and [`sync_active`] follows it each frame.
pub fn register_themes(ctx: &egui::Context) {
    ctx.set_style_of(egui::Theme::Dark, style_for(DARK));
    ctx.set_style_of(egui::Theme::Light, style_for(LIGHT));
}

/// Follow egui's currently-resolved theme: install the matching palette so the
/// application's own `theme::x()` reads agree with egui's chrome. Cheap — call
/// it each frame; switching appearance is just the next install, nothing cached.
pub fn sync_active(ctx: &egui::Context) {
    install(palette_for(ctx.theme()));
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

    /// The ink chosen for a filled pill is legible on every accent used as a
    /// fill, in both palettes — the actual defect behind "check the delete
    /// buttons", asserted rather than eyeballed.
    #[test]
    fn ink_on_accent_is_legible_in_every_palette() {
        const MIN: f32 = 4.0;
        for (name, palette) in [("dark", DARK), ("light", LIGHT)] {
            install(palette);
            for fill in [orange(), amber(), tan(), lilac(), blue(), red(), green()] {
                let ratio = contrast_ratio(ink_on(fill), fill);
                assert!(
                    ratio >= MIN,
                    "{name}: ink on {fill:?} contrast {ratio:.2} < {MIN}"
                );
            }
        }
        install(DARK);
    }

    /// On the dark palette the accents are bright, so the ink is always the dark
    /// ink — every existing filled pill is byte-identical to before the change.
    #[test]
    fn ink_on_accent_is_the_dark_ink_on_the_dark_palette() {
        install(DARK);
        for fill in [orange(), amber(), tan(), lilac(), blue(), red(), green()] {
            assert_eq!(ink_on(fill), black(), "dark pills keep their dark ink");
        }
    }

    /// Resolving to a theme installs that theme's palette; switching is live,
    /// with nothing cached to invalidate.
    #[test]
    fn sync_follows_the_resolved_theme() {
        let ctx = egui::Context::default();
        register_themes(&ctx);

        ctx.set_theme(egui::ThemePreference::Light);
        sync_active(&ctx);
        assert_eq!(
            text(),
            LIGHT.text,
            "resolving to light installs the light palette"
        );
        assert_eq!(red(), LIGHT.red);

        ctx.set_theme(egui::ThemePreference::Dark);
        sync_active(&ctx);
        assert_eq!(
            text(),
            DARK.text,
            "and back to dark on the next sync — no restart, no cache"
        );
        install(DARK);
    }

    /// The whole chain works inside a real egui frame: setting the preference,
    /// syncing, and reading the accessor all connect.
    #[test]
    fn a_surface_under_each_theme_reads_that_palette() {
        for (pref, want) in [
            (egui::ThemePreference::Light, LIGHT),
            (egui::ThemePreference::Dark, DARK),
        ] {
            let mut h = egui_kittest::Harness::builder().build_ui(move |ui| {
                register_themes(ui.ctx());
                ui.ctx().set_theme(pref);
                sync_active(ui.ctx());
            });
            h.run();
            // The harness ran the closure on this thread, so the palette it
            // installed is what the accessors now read.
            assert_eq!((text(), red()), (want.text, want.red));
        }
        install(DARK);
    }

    /// Both palettes keep the five review-board semantics **distinguishable on
    /// screen**, not merely unequal: a light palette where "will delete" and
    /// "differs" looked alike would be a deletion hazard. Asserted as a minimum
    /// perceptual distance, and looped over every palette so a sixth cannot be
    /// added without being checked. Blue joined as "resurrection" (GROUP SYNC
    /// BACK) and must stand apart from the other four.
    #[test]
    fn the_semantic_colours_stay_distinct_in_every_palette() {
        // Redmean ~40 is a conservative "clearly different on screen" floor.
        const MIN: f32 = 40.0;
        for (name, p) in [("dark", DARK), ("light", LIGHT)] {
            let semantics = [
                ("grey", p.grey),
                ("green", p.green),
                ("red", p.red),
                ("amber", p.amber),
                ("blue", p.blue),
            ];
            for i in 0..semantics.len() {
                for j in (i + 1)..semantics.len() {
                    let d = perceptual_distance(semantics[i].1, semantics[j].1);
                    assert!(
                        d >= MIN,
                        "{name}: {} and {} are too close ({d:.0} < {MIN})",
                        semantics[i].0,
                        semantics[j].0
                    );
                }
            }
        }
    }

    /// Body text is comfortably readable against the panel background in every
    /// palette — a light palette cannot ship with unreadable text.
    #[test]
    fn body_text_meets_a_contrast_threshold_in_every_palette() {
        for (name, p) in [("dark", DARK), ("light", LIGHT)] {
            let against_panel = contrast_ratio(p.text, p.panel);
            let against_bg = contrast_ratio(p.text, p.bg);
            assert!(
                against_panel >= 4.0 && against_bg >= 4.0,
                "{name}: text contrast too low (panel {against_panel:.1}, bg {against_bg:.1})"
            );
        }
    }

    #[test]
    fn every_accessor_reads_the_dark_palettes_value() {
        install(DARK); // this thread may have been left on another palette
        let rgb = Color32::from_rgb;
        assert_eq!(black(), rgb(0x00, 0x00, 0x00));
        assert_eq!(bg(), rgb(0x00, 0x00, 0x00));
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
