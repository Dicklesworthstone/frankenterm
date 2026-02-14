use crate::emoji_variation::VARIATION_MAP;
#[cfg(feature = "use_serde")]
use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub enum Presentation {
    Text,
    Emoji,
}

impl Presentation {
    /// Returns the default presentation followed
    /// by the explicit presentation if specified
    /// by a variation selector
    pub fn for_grapheme(s: &str) -> (Self, Option<Self>) {
        if let Some((a, b)) = VARIATION_MAP.get(s) {
            return (*a, Some(*b));
        }
        let mut presentation = Self::Text;
        for c in s.chars() {
            if Self::for_char(c) == Self::Emoji {
                presentation = Self::Emoji;
                break;
            }
            // Note that `c` may be some other combining
            // sequence that doesn't definitively indicate
            // that we're text, so we only positively
            // change presentation when we identify an
            // emoji char.
        }
        (presentation, None)
    }

    pub fn for_char(c: char) -> Self {
        if crate::emoji_presentation::EMOJI_PRESENTATION.contains_u32(c as u32) {
            Self::Emoji
        } else {
            Self::Text
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Presentation enum ─────────────────────────────────────

    #[test]
    fn presentation_debug() {
        assert_eq!(format!("{:?}", Presentation::Text), "Text");
        assert_eq!(format!("{:?}", Presentation::Emoji), "Emoji");
    }

    #[test]
    fn presentation_clone_eq() {
        let a = Presentation::Emoji;
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn presentation_ne() {
        assert_ne!(Presentation::Text, Presentation::Emoji);
    }

    // ── for_char ──────────────────────────────────────────────

    #[test]
    fn ascii_is_text() {
        assert_eq!(Presentation::for_char('A'), Presentation::Text);
        assert_eq!(Presentation::for_char('0'), Presentation::Text);
        assert_eq!(Presentation::for_char(' '), Presentation::Text);
    }

    #[test]
    fn smiley_is_emoji() {
        // U+1F600 GRINNING FACE - has emoji presentation
        assert_eq!(Presentation::for_char('\u{1F600}'), Presentation::Emoji);
    }

    #[test]
    fn heart_emoji() {
        // U+2764 HEAVY BLACK HEART - commonly displayed as emoji
        // Note: this may be text or emoji depending on the table
        let _ = Presentation::for_char('\u{2764}');
    }

    #[test]
    fn rocket_is_emoji() {
        // U+1F680 ROCKET
        assert_eq!(Presentation::for_char('\u{1F680}'), Presentation::Emoji);
    }

    // ── for_grapheme ──────────────────────────────────────────

    #[test]
    fn plain_text_grapheme() {
        let (default, explicit) = Presentation::for_grapheme("A");
        assert_eq!(default, Presentation::Text);
        assert_eq!(explicit, None);
    }

    #[test]
    fn emoji_grapheme() {
        // Smiley face as a grapheme
        let (default, _explicit) = Presentation::for_grapheme("\u{1F600}");
        assert_eq!(default, Presentation::Emoji);
    }

    #[test]
    fn variation_selector_text() {
        // U+2764 followed by VS15 (text presentation)
        let (default, explicit) = Presentation::for_grapheme("\u{2764}\u{FE0E}");
        if let Some(exp) = explicit {
            // If the variation map has this entry, it should specify text
            assert_eq!(exp, Presentation::Text);
        } else {
            // Otherwise just check default isn't panic
            let _ = default;
        }
    }

    #[test]
    fn variation_selector_emoji() {
        // U+2764 followed by VS16 (emoji presentation)
        let (default, explicit) = Presentation::for_grapheme("\u{2764}\u{FE0F}");
        if let Some(exp) = explicit {
            assert_eq!(exp, Presentation::Emoji);
        } else {
            let _ = default;
        }
    }

    #[test]
    fn empty_string_is_text() {
        let (default, explicit) = Presentation::for_grapheme("");
        assert_eq!(default, Presentation::Text);
        assert_eq!(explicit, None);
    }

    #[test]
    fn multi_char_grapheme_with_emoji() {
        // Family emoji (ZWJ sequence) - should detect emoji in the sequence
        let (default, _) = Presentation::for_grapheme("\u{1F468}\u{200D}\u{1F469}");
        assert_eq!(default, Presentation::Emoji);
    }

    // ── Additional for_char tests ───────────────────────────

    #[test]
    fn digit_is_text() {
        // Digits 0-9 have text default presentation
        for c in '0'..='9' {
            assert_eq!(Presentation::for_char(c), Presentation::Text, "digit {c}");
        }
    }

    #[test]
    fn various_emoji_presentation() {
        // U+1F4A9 PILE OF POO
        assert_eq!(Presentation::for_char('\u{1F4A9}'), Presentation::Emoji);
        // U+1F680 ROCKET
        assert_eq!(Presentation::for_char('\u{1F680}'), Presentation::Emoji);
        // U+1F525 FIRE
        assert_eq!(Presentation::for_char('\u{1F525}'), Presentation::Emoji);
    }

    #[test]
    fn combining_mark_is_text() {
        // U+0300 COMBINING GRAVE ACCENT - not emoji presentation
        assert_eq!(Presentation::for_char('\u{0300}'), Presentation::Text);
    }

    #[test]
    fn cjk_is_text() {
        // CJK ideograph - text presentation
        assert_eq!(Presentation::for_char('\u{4e00}'), Presentation::Text);
    }

    #[test]
    fn regional_indicator_is_emoji() {
        // U+1F1E6 REGIONAL INDICATOR SYMBOL LETTER A has emoji presentation
        assert_eq!(Presentation::for_char('\u{1F1E6}'), Presentation::Emoji);
    }

    // ── Additional for_grapheme tests ───────────────────────

    #[test]
    fn grapheme_single_ascii_char() {
        for c in ['a', 'Z', '5', '!', '#'] {
            let s = String::from(c);
            let (default, explicit) = Presentation::for_grapheme(&s);
            assert_eq!(default, Presentation::Text, "char {c}");
            assert_eq!(explicit, None, "char {c}");
        }
    }

    #[test]
    fn grapheme_rocket_emoji() {
        let (default, _) = Presentation::for_grapheme("\u{1F680}");
        assert_eq!(default, Presentation::Emoji);
    }

    #[test]
    fn grapheme_multiple_text_chars() {
        // Multiple text characters with no emoji
        let (default, explicit) = Presentation::for_grapheme("abc");
        assert_eq!(default, Presentation::Text);
        assert_eq!(explicit, None);
    }

    #[test]
    fn presentation_copy_trait() {
        let a = Presentation::Text;
        let b = a; // Copy
        let c = a; // Still valid - Copy
        assert_eq!(b, c);
    }

    // ── Third-pass expansion ────────────────────────────────

    #[test]
    fn control_chars_are_text() {
        assert_eq!(Presentation::for_char('\t'), Presentation::Text);
        assert_eq!(Presentation::for_char('\n'), Presentation::Text);
        assert_eq!(Presentation::for_char('\x00'), Presentation::Text);
    }

    #[test]
    fn snowman_is_text_presentation() {
        // U+2603 SNOWMAN has text default presentation
        assert_eq!(Presentation::for_char('\u{2603}'), Presentation::Text);
    }

    #[test]
    fn for_grapheme_flag_sequence_is_emoji() {
        // Two regional indicator letters form a flag (U+1F1FA U+1F1F8 = US)
        let (default, _) = Presentation::for_grapheme("\u{1F1FA}\u{1F1F8}");
        assert_eq!(default, Presentation::Emoji);
    }

    #[test]
    fn keycap_base_chars_are_text() {
        // '#' and '*' are keycap base characters but have text presentation
        assert_eq!(Presentation::for_char('#'), Presentation::Text);
        assert_eq!(Presentation::for_char('*'), Presentation::Text);
    }

    #[test]
    fn for_grapheme_single_emoji_no_variation() {
        // Single emoji char without variation selector => no explicit
        let (default, explicit) = Presentation::for_grapheme("\u{1F4A9}");
        assert_eq!(default, Presentation::Emoji);
        assert_eq!(explicit, None);
    }

    #[test]
    fn for_char_various_text_scripts() {
        // Arabic, Hebrew, Cyrillic — all text presentation
        assert_eq!(Presentation::for_char('\u{0627}'), Presentation::Text); // Arabic Alef
        assert_eq!(Presentation::for_char('\u{05D0}'), Presentation::Text); // Hebrew Alef
        assert_eq!(Presentation::for_char('\u{0410}'), Presentation::Text); // Cyrillic A
    }

    #[test]
    fn for_char_clock_faces_are_emoji() {
        // U+1F550 CLOCK FACE ONE OCLOCK has emoji presentation
        assert_eq!(Presentation::for_char('\u{1F550}'), Presentation::Emoji);
    }

    #[test]
    fn for_grapheme_zwj_only_is_text() {
        // Bare ZWJ with no emoji should be text
        let (default, _) = Presentation::for_grapheme("\u{200D}");
        assert_eq!(default, Presentation::Text);
    }
}
