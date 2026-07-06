//! Phosphor icon glyphs. The `egui-phosphor` crate targets an older egui, so we
//! vendor the regular Phosphor font (MIT licensed, © Phosphor Icons) and the
//! handful of codepoints we use, and register it against egui 0.35 directly.
//!
//! Icons live in the Unicode private-use area, so they never collide with real
//! text glyphs; the font is appended as a fallback in the proportional family.

use egui::{FontData, FontDefinitions, FontFamily};
use std::sync::Arc;

const PHOSPHOR: &[u8] = include_bytes!("../assets/Phosphor.ttf");

pub const PLUS: &str = "\u{E3D4}";
pub const FOLDER_OPEN: &str = "\u{E256}";
pub const REFRESH: &str = "\u{E094}"; // arrows-clockwise
pub const PENCIL: &str = "\u{E3B4}"; // pencil-simple
pub const RELOCATE: &str = "\u{E0A0}"; // arrows-left-right
pub const COPY: &str = "\u{E1CA}";
pub const TRASH: &str = "\u{E4A6}";
pub const GEAR: &str = "\u{E270}";
pub const LOCK: &str = "\u{E2FA}"; // closed padlock
pub const LOCK_OPEN: &str = "\u{E300}"; // open padlock
pub const SEARCH: &str = "\u{E30C}"; // magnifying-glass
pub const STAR: &str = "\u{E46A}";
pub const CARET_LEFT: &str = "\u{E138}";
pub const CARET_RIGHT: &str = "\u{E13A}";
pub const IMAGE: &str = "\u{E2CC}"; // image-square
pub const X: &str = "\u{E4F6}";
pub const ARROW_RIGHT: &str = "\u{E06C}";
pub const CHECK: &str = "\u{E182}";

/// Register the Phosphor font on the context so the glyph constants render.
pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "phosphor".to_owned(),
        Arc::new(FontData::from_static(PHOSPHOR)),
    );
    if let Some(family) = fonts.families.get_mut(&FontFamily::Proportional) {
        family.push("phosphor".to_owned());
    }
    ctx.set_fonts(fonts);
}
