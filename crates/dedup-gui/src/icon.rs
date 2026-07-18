//! Fonts: the condensed LCARS-style body face plus the Phosphor icon glyphs.
//!
//! The `egui-phosphor` crate targets an older egui, so we vendor the regular
//! Phosphor font (MIT licensed, © Phosphor Icons) and register it against egui
//! 0.35 directly. Icons live in the Unicode private-use area, so they never
//! collide with real text glyphs; the font is appended as a fallback in the
//! proportional family.
//!
//! Body text uses **DejaVu Sans Condensed** (Bitstream Vera license — see
//! `assets/fonts/DejaVu-LICENSE.txt`), a narrow face that gives the UI its
//! technical LCARS character. It's installed as the *primary* proportional font.

use egui::{FontData, FontDefinitions, FontFamily};
use std::sync::Arc;

const PHOSPHOR: &[u8] = include_bytes!("../assets/Phosphor.ttf");
const CONDENSED: &[u8] = include_bytes!("../assets/fonts/DejaVuSansCondensed.ttf");

pub const PLUS: &str = "\u{E3D4}";
pub const FOLDER_OPEN: &str = "\u{E256}";
pub const REFRESH: &str = "\u{E094}"; // arrows-clockwise
pub const PENCIL: &str = "\u{E3B4}"; // pencil-simple
pub const RELOCATE: &str = "\u{E0A0}"; // arrows-left-right
pub const COPY: &str = "\u{E1CA}";
pub const TRASH: &str = "\u{E4A6}";
pub const GEAR: &str = "\u{E270}";
pub const LIGHTNING: &str = "\u{E2DE}"; // lightning bolt (speed)
pub const LOCK: &str = "\u{E2FA}"; // closed padlock
pub const LOCK_OPEN: &str = "\u{E300}"; // open padlock
pub const SEARCH: &str = "\u{E30C}"; // magnifying-glass
pub const STAR: &str = "\u{E46A}";
pub const CARET_LEFT: &str = "\u{E138}";
pub const CARET_RIGHT: &str = "\u{E13A}";
pub const CARET_UP: &str = "\u{E13C}";
pub const CARET_DOWN: &str = "\u{E136}";
pub const IMAGE: &str = "\u{E2CC}"; // image-square
pub const X: &str = "\u{E4F6}";
pub const ARROW_RIGHT: &str = "\u{E06C}";
pub const CHECK: &str = "\u{E182}";

/// Register the condensed body font and the Phosphor icon font on the context.
pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "condensed".to_owned(),
        Arc::new(FontData::from_static(CONDENSED)),
    );
    fonts.font_data.insert(
        "phosphor".to_owned(),
        Arc::new(FontData::from_static(PHOSPHOR)),
    );
    if let Some(family) = fonts.families.get_mut(&FontFamily::Proportional) {
        // Condensed body face first (primary), then the platform default fonts,
        // then Phosphor as the fallback that supplies our PUA icon glyphs.
        family.insert(0, "condensed".to_owned());
        family.push("phosphor".to_owned());
    }
    ctx.set_fonts(fonts);
}
