//! ft-7cy5r: the char-props tables are Unicode 17.0.0.

use frankenterm_char_props::emoji_presentation::EMOJI_PRESENTATION;
use frankenterm_char_props::emoji_variation::WCWIDTH_TABLE;
use frankenterm_char_props::widechar_width::{WcLookupTable, WcWidth};

/// The Unicode 17.0 emoji of the operator's T0 pool, unassigned in Unicode 16.
const UNICODE_17_EMOJI: [char; 7] = [
    '\u{1F6D8}',
    '\u{1FA8A}',
    '\u{1FA8E}',
    '\u{1FAC8}',
    '\u{1FACD}',
    '\u{1FAEA}',
    '\u{1FAEF}',
];

#[test]
fn unicode_17_emoji_are_double_width_emoji_presentation() {
    for c in UNICODE_17_EMOJI {
        assert_eq!(WcWidth::from_char(c), WcWidth::Two, "U+{:X}", c as u32);
        assert_eq!(WCWIDTH_TABLE.classify(c), WcWidth::Two, "U+{:X}", c as u32);
        assert!(
            EMOJI_PRESENTATION.contains_char(c),
            "U+{:X} has Emoji_Presentation",
            c as u32
        );
    }
}

/// The precomputed BMP table in emoji_variation.rs is exactly what
/// `WcLookupTable::new()` builds from widechar_width.rs's range tables, so a
/// regeneration of one cannot drift from the other.
#[test]
fn the_precomputed_bmp_table_matches_the_range_tables() {
    let built = WcLookupTable::new();
    for (cp, (stored, expected)) in WCWIDTH_TABLE
        .table
        .iter()
        .zip(built.table.iter())
        .enumerate()
    {
        assert_eq!(stored, expected, "U+{cp:04X}");
    }
}

#[test]
fn unicode_17_bmp_combining_marks_are_combining() {
    for c in ['\u{1ACF}', '\u{1ADD}', '\u{1AE0}', '\u{1AEB}'] {
        assert_eq!(
            WCWIDTH_TABLE.classify(c),
            WcWidth::Combining,
            "U+{:X}",
            c as u32
        );
    }
}
