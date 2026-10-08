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
        Self::for_grapheme_with(s, vs_skip_enabled())
    }

    /// [`Self::for_grapheme`], skipping the variation map for a grapheme
    /// without a variation selector when `skip` holds (ft-yg1lt). Every key
    /// of VARIATION_MAP holds VS15 or VS16 (a test walks them all), so such a
    /// grapheme is no key: hashing all of it only to miss cost the T0 parse
    /// thread more than the one table lookup a single code point now takes.
    /// Both answer the same for every string.
    fn for_grapheme_with(s: &str, skip: bool) -> (Self, Option<Self>) {
        if !skip {
            return Self::for_grapheme_hashed(s);
        }
        // One pass: a T0 emoji is one decode and one table lookup. A
        // selector, rare in output, sends the grapheme to the map, a few
        // nanoseconds later than looking it up directly would.
        let mut presentation = Self::Text;
        for c in s.chars() {
            if matches!(c, '\u{FE0E}' | '\u{FE0F}') {
                return Self::for_grapheme_hashed(s);
            }
            if presentation == Self::Text && Self::for_char(c) == Self::Emoji {
                presentation = Self::Emoji;
            }
        }
        (presentation, None)
    }

    /// The variation map first, then a scan for an emoji code point: how
    /// every grapheme was looked up before ft-yg1lt.
    fn for_grapheme_hashed(s: &str) -> (Self, Option<Self>) {
        if let Some((a, b)) = VARIATION_MAP.get(s) {
            return (*a, Some(*b));
        }
        Self::scan(s)
    }

    /// The default presentation of a grapheme no variation sequence names:
    /// emoji when one of its code points is.
    fn scan(s: &str) -> (Self, Option<Self>) {
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

/// Whether [`Presentation::for_grapheme`] skips the variation map for a
/// grapheme without a variation selector (ft-yg1lt): on unless
/// `FT_PRESENTATION_VS_SKIP=0`, the A/B arm and a rollback. Both answer the
/// same. Resolved once per process.
fn vs_skip_enabled() -> bool {
    #[cfg(feature = "std")]
    {
        static SKIP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *SKIP.get_or_init(|| !std::env::var_os("FT_PRESENTATION_VS_SKIP").is_some_and(|v| v == "0"))
    }
    #[cfg(not(feature = "std"))]
    true
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
        assert_eq!(default, Presentation::Text);
        assert_eq!(explicit, Some(Presentation::Text));
    }

    #[test]
    fn variation_selector_emoji() {
        // U+2764 followed by VS16 (emoji presentation)
        let (default, explicit) = Presentation::for_grapheme("\u{2764}\u{FE0F}");
        assert_eq!(default, Presentation::Text);
        assert_eq!(explicit, Some(Presentation::Emoji));
    }

    #[test]
    fn generated_variation_map_entries_are_lookup_reachable() {
        for (grapheme, expected) in VARIATION_MAP.entries() {
            assert_eq!(
                VARIATION_MAP.get(*grapheme),
                Some(expected),
                "generated PHF entry for {grapheme:?} is unreachable"
            );
            assert_eq!(
                Presentation::for_grapheme(grapheme),
                (expected.0, Some(expected.1)),
                "generated presentation for {grapheme:?} is not honored"
            );
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

    /// ft-yg1lt: what one presentation lookup costs with the variation map
    /// skipped and without, on the T0 pool (its 1,376 emoji and 71 ASCII
    /// characters, each as a grapheme) and on the map's own keys, alternating
    /// arms, best of 5. A measurement, not a gate:
    /// cargo test --profile release-perf -p frankenterm-char-props --lib -- --ignored presentation_lookup_cost --nocapture
    #[test]
    #[ignore = "a throughput measurement; run it in release"]
    fn presentation_lookup_cost() {
        let pool: Vec<String> = [
            (0x1F600, 0x1F64F),
            (0x1F300, 0x1F5FF),
            (0x1F680, 0x1F6FF),
            (0x1F900, 0x1F9FF),
            (0x1FA70, 0x1FAFF),
        ]
        .iter()
        .flat_map(|&(start, end)| (start..=end).filter_map(char::from_u32))
        .chain("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!@#%^&*()".chars())
        .map(|c| c.to_string())
        .collect();
        let keys: Vec<String> = VARIATION_MAP.keys().map(|key| key.to_string()).collect();
        for (name, set) in [("T0 pool", &pool), ("variation keys", &keys)] {
            let mut best = [f64::INFINITY; 2];
            for _ in 0..5 {
                for (arm, &skip) in [false, true].iter().enumerate() {
                    let start = std::time::Instant::now();
                    for _ in 0..200 {
                        for s in set {
                            std::hint::black_box(Presentation::for_grapheme_with(
                                std::hint::black_box(s),
                                skip,
                            ));
                        }
                    }
                    best[arm] = best[arm].min(start.elapsed().as_secs_f64());
                }
            }
            let calls = (200 * set.len()) as f64;
            eprintln!(
                "[BENCH] presentation lookup, {}: hashed {:.1} ns/call, skipping {:.1} ns/call, {:.2}x",
                name,
                best[0] * 1e9 / calls,
                best[1] * 1e9 / calls,
                best[0] / best[1]
            );
        }
    }

    /// ft-yg1lt: the premise of skipping the variation map. Every key holds
    /// a variation selector, VS15 or VS16, so a grapheme without one is no
    /// key. The keys are the variation sequences they are generated from: a
    /// base, then the selector.
    #[test]
    fn every_variation_map_key_holds_a_variation_selector() {
        let mut keys = 0;
        for key in VARIATION_MAP.keys() {
            keys += 1;
            assert!(
                key.chars().any(|c| matches!(c, '\u{FE0E}' | '\u{FE0F}')),
                "{:?}",
                key
            );
            let chars: Vec<char> = key.chars().collect();
            assert_eq!(chars.len(), 2, "{:?}", key);
            assert!(matches!(chars[1], '\u{FE0E}' | '\u{FE0F}'), "{:?}", key);
        }
        assert!(keys > 600, "{} keys", keys);
    }

    /// ft-yg1lt: skipping the variation map answers as the hashed lookup,
    /// exhaustively where it can:
    /// - every key, its base alone, and its base with the other selector;
    /// - every Unicode scalar value alone (all of EMOJI_PRESENTATION, the
    ///   T0 pool's 1,376 emoji);
    /// - every scalar of the planes the keys and emoji live in (below
    ///   U+3400, and U+1F000..=U+1FFFF), followed by VS15, VS16, ZWJ or the
    ///   keycap mark;
    /// - 200,000 random sequences of one to six code points mixing those
    ///   with ZWJ joins, keycaps, regional indicators, skin tones, CJK,
    ///   combining marks and arbitrary scalars.
    #[test]
    fn skipping_the_variation_map_answers_as_the_hashed_lookup() {
        let agree = |s: &str| {
            assert_eq!(
                Presentation::for_grapheme_with(s, true),
                Presentation::for_grapheme_with(s, false),
                "{:?}",
                s
            );
        };
        let mut bases = Vec::new();
        for key in VARIATION_MAP.keys() {
            agree(key);
            let base: String = key
                .chars()
                .filter(|c| !matches!(c, '\u{FE0E}' | '\u{FE0F}'))
                .collect();
            agree(&base);
            let other = if key.contains('\u{FE0F}') {
                '\u{FE0E}'
            } else {
                '\u{FE0F}'
            };
            agree(&format!("{}{}", base, other));
            bases.push(base);
        }
        let mut buf = [0u8; 4];
        for scalar in (0..=0x10FFFF_u32).filter_map(char::from_u32) {
            agree(scalar.encode_utf8(&mut buf));
            if scalar < '\u{3400}' || ('\u{1F000}'..='\u{1FFFF}').contains(&scalar) {
                for tail in ['\u{FE0E}', '\u{FE0F}', '\u{200D}', '\u{20E3}'] {
                    agree(&format!("{}{}", scalar, tail));
                }
            }
        }
        let pieces: Vec<String> = [
            "#",
            "*",
            "0",
            "7",
            "a",
            "\u{FE0E}",
            "\u{FE0F}",
            "\u{200D}",
            "\u{20E3}",
            "\u{1F1FA}",
            "\u{1F1F8}",
            "\u{1F3FB}",
            "\u{1F3FF}",
            "\u{1F600}",
            "\u{1F469}",
            "\u{2764}",
            "\u{263A}",
            "\u{2603}",
            "\u{754C}",
            "\u{301}",
            "\u{1FAF1}",
            "\u{1F9D1}",
        ]
        .iter()
        .map(|s| s.to_string())
        .chain(bases.iter().cloned())
        .collect();
        let mut seed = 0x0009_1e17_u64;
        let mut next = move |bound: usize| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) as usize) % bound
        };
        for _ in 0..200_000 {
            let mut s = String::new();
            for _ in 0..1 + next(6) {
                if next(8) == 0 {
                    if let Some(c) = char::from_u32(next(0x110000) as u32) {
                        s.push(c);
                    }
                } else {
                    s.push_str(&pieces[next(pieces.len())]);
                }
            }
            agree(&s);
        }
    }
}
