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
pub const TEXT: Color32 = Color32::from_rgb(0xEB, 0xD0, 0xA0);
pub const PANEL: Color32 = Color32::from_rgb(0x0A, 0x08, 0x0C);
/// Subtle warm outline that separates dark image content from the dark panels.
pub const HAIRLINE: Color32 = Color32::from_rgb(0x5C, 0x4E, 0x40);

/// Large corner radius gives widgets the rounded LCARS block look.
pub const PILL: CornerRadius = CornerRadius::same(12);

/// A bold-bordered LCARS section container in the given accent color, used to
/// group a row of related controls.
pub fn section(color: Color32) -> egui::Frame {
    egui::Frame::new()
        .fill(PANEL)
        .corner_radius(PILL)
        .stroke(Stroke::new(2.0, color))
        .inner_margin(8.0)
        .outer_margin(egui::Margin {
            left: 0,
            right: 0,
            top: 0,
            bottom: 8,
        })
}

fn pill(bg: Color32, fg: Color32) -> WidgetVisuals {
    WidgetVisuals {
        bg_fill: bg,
        weak_bg_fill: bg,
        bg_stroke: Stroke::NONE,
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
