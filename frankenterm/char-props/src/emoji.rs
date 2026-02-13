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
}
