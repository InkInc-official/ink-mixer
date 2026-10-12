//! The font bundled with the GUI (ADR-0006).
//!
//! Noto Sans JP Regular is embedded in the executable, so Japanese text shows
//! the same on every PC, including Linux systems without a Japanese font.
//! egui's own fonts are not built in (the `default_fonts` feature is off), so
//! this font is used for both proportional and monospace text. The font, its
//! license, and its source are in `assets/fonts/`.

use std::sync::Arc;

use eframe::egui::{Context, FontData, FontDefinitions, FontFamily};

/// The name the font is registered under in egui.
pub const FONT_NAME: &str = "Noto Sans JP";

/// The font file, unmodified (see `assets/fonts/README.md`).
static NOTO_SANS_JP: &[u8] = include_bytes!("../../assets/fonts/NotoSansJP-Regular.otf");

/// Returns the font definitions with Noto Sans JP first in every family.
pub fn font_definitions() -> FontDefinitions {
    // Empty without the `default_fonts` feature; otherwise egui's fonts stay
    // after ours as fallbacks.
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        FONT_NAME.to_owned(),
        Arc::new(FontData::from_static(NOTO_SANS_JP)),
    );
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, FONT_NAME.to_owned());
    }
    fonts
}

/// Installs the bundled font into `ctx`. Call once, when the window is
/// created.
pub fn install(ctx: &Context) {
    ctx.set_fonts(font_definitions());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_font_is_the_recorded_opentype_file() {
        // OpenType with CFF outlines starts with "OTTO".
        assert_eq!(&NOTO_SANS_JP[..4], b"OTTO");
        // The size recorded in assets/fonts/README.md.
        assert_eq!(NOTO_SANS_JP.len(), 4_533_028);
    }

    #[test]
    fn noto_sans_jp_comes_first_in_every_family() {
        let fonts = font_definitions();
        assert!(fonts.font_data.contains_key(FONT_NAME));
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            let names = &fonts.families[&family];
            assert_eq!(names.first().map(String::as_str), Some(FONT_NAME));
        }
    }
}
