#[cfg(feature = "std")]
use std::collections::HashMap;
#[cfg(feature = "std")]
use std::sync::LazyLock;

#[cfg(feature = "std")]
pub static NERD_FONTS: LazyLock<HashMap<&'static str, char>> = LazyLock::new(build_map);

pub use crate::nerdfonts_data::NERD_FONT_GLYPHS;

#[cfg(feature = "std")]
fn build_map() -> HashMap<&'static str, char> {
    crate::nerdfonts_data::NERD_FONT_GLYPHS
        .iter()
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nerd_fonts_map_is_not_empty() {
        assert!(!NERD_FONTS.is_empty());
    }

    #[test]
    fn nerd_fonts_map_size_matches_glyphs() {
        // The map should have the same number of entries as the source data
        // (assuming no duplicate names in the source)
        assert_eq!(NERD_FONTS.len(), NERD_FONT_GLYPHS.len());
    }

    #[test]
    fn nerd_fonts_glyphs_array_not_empty() {
        assert!(!NERD_FONT_GLYPHS.is_empty());
    }

    #[test]
    fn nerd_fonts_lookup_returns_char() {
        // Pick the first entry and verify it roundtrips
        let (name, expected_char) = NERD_FONT_GLYPHS[0];
        let got = NERD_FONTS.get(name);
        assert_eq!(got, Some(&expected_char));
    }

    #[test]
    fn nerd_fonts_nonexistent_returns_none() {
        assert!(NERD_FONTS.get("__definitely_not_a_real_glyph__").is_none());
    }
}
