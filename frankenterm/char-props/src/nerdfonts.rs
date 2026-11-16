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

    #[test]
    fn nerd_fonts_last_entry_roundtrips() {
        let (name, expected_char) = NERD_FONT_GLYPHS.last().unwrap();
        let got = NERD_FONTS.get(name);
        assert_eq!(got, Some(expected_char));
    }

    #[test]
    fn nerd_fonts_middle_entry_roundtrips() {
        let mid = NERD_FONT_GLYPHS.len() / 2;
        let (name, expected_char) = NERD_FONT_GLYPHS[mid];
        let got = NERD_FONTS.get(name);
        assert_eq!(got, Some(&expected_char));
    }

    #[test]
    fn nerd_font_glyph_names_are_nonempty() {
        for (name, _) in NERD_FONT_GLYPHS.iter() {
            assert!(!name.is_empty(), "found empty glyph name");
        }
    }

    #[test]
    fn nerd_font_glyph_chars_are_nonzero() {
        for (name, ch) in NERD_FONT_GLYPHS.iter() {
            assert_ne!(*ch, '\0', "glyph {name} has null char");
        }
    }

    #[test]
    fn nerd_fonts_map_all_values_match_glyphs() {
        // Every entry in NERD_FONT_GLYPHS should be findable in the HashMap
        for (name, expected) in NERD_FONT_GLYPHS.iter() {
            let got = NERD_FONTS.get(name);
            assert_eq!(got, Some(expected), "mismatch for {name}");
        }
    }

    #[test]
    fn nerd_fonts_empty_string_returns_none() {
        assert!(NERD_FONTS.get("").is_none());
    }

    #[test]
    fn nerd_fonts_case_sensitive() {
        // Glyph names should be case-sensitive
        let (name, _) = NERD_FONT_GLYPHS[0];
        // If name has letters, an upper/lower swap should not match
        if name.chars().any(|c| c.is_ascii_alphabetic()) {
            let swapped: String = name
                .chars()
                .map(|c| {
                    if c.is_ascii_lowercase() {
                        c.to_ascii_uppercase()
                    } else if c.is_ascii_uppercase() {
                        c.to_ascii_lowercase()
                    } else {
                        c
                    }
                })
                .collect();
            if swapped != name {
                assert!(
                    NERD_FONTS.get(swapped.as_str()).is_none()
                        || NERD_FONTS.get(swapped.as_str()) == NERD_FONTS.get(name),
                    "case-swapped name should not match a different glyph"
                );
            }
        }
    }

    // ── Third-pass expansion ────────────────────────────────

    #[test]
    fn nerd_font_glyph_names_are_unique() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for (name, _) in NERD_FONT_GLYPHS.iter() {
            assert!(seen.insert(*name), "duplicate glyph name: {name}");
        }
    }

    #[test]
    fn nerd_font_glyph_chars_above_ascii() {
        // All nerd font glyphs should be outside the basic ASCII range
        for (name, ch) in NERD_FONT_GLYPHS.iter() {
            assert!(
                *ch as u32 > 0x7F,
                "glyph {name} char U+{:04X} is within ASCII range",
                *ch as u32
            );
        }
    }

    #[test]
    fn nerd_font_first_and_last_differ() {
        let first = NERD_FONT_GLYPHS.first().unwrap();
        let last = NERD_FONT_GLYPHS.last().unwrap();
        assert_ne!(first.0, last.0, "first and last glyph names should differ");
    }

    #[test]
    fn nerd_fonts_map_len_equals_glyphs_len() {
        // Confirm HashMap has no collisions (same as array length)
        assert_eq!(NERD_FONTS.len(), NERD_FONT_GLYPHS.len());
    }

    #[test]
    fn nerd_font_quarter_entry_roundtrips() {
        let q = NERD_FONT_GLYPHS.len() / 4;
        let (name, expected_char) = NERD_FONT_GLYPHS[q];
        assert_eq!(NERD_FONTS.get(name), Some(&expected_char));
    }

    #[test]
    fn nerd_font_three_quarter_entry_roundtrips() {
        let q = (NERD_FONT_GLYPHS.len() * 3) / 4;
        let (name, expected_char) = NERD_FONT_GLYPHS[q];
        assert_eq!(NERD_FONTS.get(name), Some(&expected_char));
    }
}
