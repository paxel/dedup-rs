//! LCARS-inspired dark theme: black backdrop, warm amber text, and rounded
//! colored "pill" widgets in the classic orange / lilac / periwinkle palette.

use egui::style::WidgetVisuals;
use egui::{Color32, CornerRadius, Stroke, Style, Visuals};

pub const BLACK: Color32 = Color32::from_rgb(0x00, 0x00, 0x00);
pub const ORANGE: Color32 = Color32::from_rgb(0xFF, 0x99, 0x00);
pub const AMBER: Color32 = Color32::from_rgb(0xFF, 0xCC, 0x66);
pub const TAN: Color32 = Color32::from_rgb(0xFF, 0xCC, 0x99);
pub const LILAC: Color32 = Color32::from_rgb(0xCC, 0x99, 0xFF);
pub const BLUE: Color32 = Color32::from_rgb(0x66, 0x99, 0xFF);
pub const RED: Color32 = Color32::from_rgb(0xE0, 0x66, 0x55);
/// Muted LCARS green for "added" review rows (kept distinct from the amber/red
/// pills so the review table's colour cue reads clearly on the black backdrop).
pub const GREEN: Color32 = Color32::from_rgb(0x66, 0xCC, 0x77);
/// Dim warm grey for "unchanged" review rows — present but visually recessive.
pub const GREY: Color32 = Color32::from_rgb(0x8A, 0x82, 0x74);
pub const TEXT: Color32 = Color32::from_rgb(0xEB, 0xD0, 0xA0);
pub const PANEL: Color32 = Color32::from_rgb(0x0A, 0x08, 0x0C);
/// Subtle warm outline that separates dark image content from the dark panels.
pub const HAIRLINE: Color32 = Color32::from_rgb(0x5C, 0x4E, 0x40);

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

/// Install the theme on the given context.
pub fn apply(ctx: &egui::Context) {
    let mut style = Style::default();
    let mut v = Visuals::dark();

    v.dark_mode = true;
    v.window_fill = BLACK;
    v.panel_fill = BLACK;
    v.faint_bg_color = PANEL;
    v.extreme_bg_color = Color32::from_rgb(0x12, 0x0D, 0x16);
    v.override_text_color = Some(TEXT);
    v.hyperlink_color = BLUE;
    v.window_stroke = Stroke::new(1.5, ORANGE);
    v.selection.bg_fill = Color32::from_rgb(0x24, 0x33, 0x5C);
    v.selection.stroke = Stroke::new(1.0, BLUE);

    // Buttons: orange at rest, amber on hover, tan when pressed, lilac when open.
    v.widgets.inactive = pill(ORANGE, BLACK);
    v.widgets.hovered = pill(AMBER, BLACK);
    v.widgets.active = pill(TAN, BLACK);
    v.widgets.open = pill(LILAC, BLACK);
    v.widgets.noninteractive.corner_radius = PILL;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x3A, 0x2A, 0x1E));
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);

    style.visuals = v;
    style.spacing.button_padding = egui::vec2(12.0, 6.0);
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    // Solid (non-floating) scrollbars: overflowing lists get a permanent bar
    // instead of a hover-only overlay, so long result pages are visibly
    // scrollable.
    style.spacing.scroll = egui::style::ScrollStyle::solid();
    ctx.all_styles_mut(move |s| *s = style.clone());
}
