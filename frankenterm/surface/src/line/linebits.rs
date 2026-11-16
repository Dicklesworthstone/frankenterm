use bitflags::bitflags;
#[cfg(feature = "use_serde")]
use serde::{Deserialize, Serialize};

bitflags! {
    #[cfg_attr(feature="use_serde", derive(Serialize, Deserialize))]
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub(crate) struct LineBits : u16 {
        const NONE = 0;
        /// The line contains 1+ cells with explicit hyperlinks set
        const HAS_HYPERLINK = 1<<1;
        /// true if we have scanned for implicit hyperlinks
        const SCANNED_IMPLICIT_HYPERLINKS = 1<<2;
        /// true if we found implicit hyperlinks in the last scan
        const HAS_IMPLICIT_HYPERLINKS = 1<<3;

        /// true if this line should be displayed with
        /// in double-width
        const DOUBLE_WIDTH = 1<<4;

        /// true if this line should be displayed
        /// as double-height top-half
        const DOUBLE_HEIGHT_TOP = 1<<5;

        /// true if this line should be displayed
        /// as double-height bottom-half
        const DOUBLE_HEIGHT_BOTTOM = 1<<6;

        const DOUBLE_WIDTH_HEIGHT_MASK =
            Self::DOUBLE_WIDTH.bits() |
            Self::DOUBLE_HEIGHT_TOP.bits() |
            Self::DOUBLE_HEIGHT_BOTTOM.bits();

        /// true if the line should have the bidi algorithm
        /// applied as part of presentation.
        /// This corresponds to the "implicit" bidi modes
        /// described in
        /// <https://terminal-wg.pages.freedesktop.org/bidi/recommendation/basic-modes.html>
        const BIDI_ENABLED = 1<<0;

        /// true if the line base direction is RTL.
        /// When BIDI_ENABLED is also true, this is passed to the bidi algorithm.
        /// When rendering, the line will be rendered from RTL.
        const RTL = 1<<7;

        /// true if the direction for the line should be auto-detected
        /// when BIDI_ENABLED is also true.
        /// If false, the direction is taken from the RTL bit only.
        /// Otherwise, the auto-detect direction is used, falling back
        /// to the direction specified by the RTL bit.
        const AUTO_DETECT_DIRECTION = 1<<8;
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use alloc::format;

    #[test]
    fn none_is_empty() {
        let bits = LineBits::NONE;
        assert!(bits.is_empty());
        assert_eq!(bits.bits(), 0);
    }

    #[test]
    fn individual_flags_are_distinct() {
        let flags = [
            LineBits::HAS_HYPERLINK,
            LineBits::SCANNED_IMPLICIT_HYPERLINKS,
            LineBits::HAS_IMPLICIT_HYPERLINKS,
            LineBits::DOUBLE_WIDTH,
            LineBits::DOUBLE_HEIGHT_TOP,
            LineBits::DOUBLE_HEIGHT_BOTTOM,
            LineBits::BIDI_ENABLED,
            LineBits::RTL,
            LineBits::AUTO_DETECT_DIRECTION,
        ];
        for (i, a) in flags.iter().enumerate() {
            for (j, b) in flags.iter().enumerate() {
                if i != j {
                    // No two individual flags should be equal
                    assert_ne!(a, b, "flags[{}] should differ from flags[{}]", i, j);
                    // Individual flags should not overlap
                    assert!(
                        (*a & *b).is_empty(),
                        "flags[{}] and flags[{}] should not overlap",
                        i,
                        j
                    );
                }
            }
        }
    }

    #[test]
    fn double_width_height_mask_covers_three_flags() {
        let mask = LineBits::DOUBLE_WIDTH_HEIGHT_MASK;
        assert!(mask.contains(LineBits::DOUBLE_WIDTH));
        assert!(mask.contains(LineBits::DOUBLE_HEIGHT_TOP));
        assert!(mask.contains(LineBits::DOUBLE_HEIGHT_BOTTOM));
        // Should not contain unrelated flags
        assert!(!mask.contains(LineBits::HAS_HYPERLINK));
        assert!(!mask.contains(LineBits::BIDI_ENABLED));
        assert!(!mask.contains(LineBits::RTL));
    }

    #[test]
    fn bitwise_or_combines_flags() {
        let bits = LineBits::HAS_HYPERLINK | LineBits::RTL;
        assert!(bits.contains(LineBits::HAS_HYPERLINK));
        assert!(bits.contains(LineBits::RTL));
        assert!(!bits.contains(LineBits::DOUBLE_WIDTH));
    }

    #[test]
    fn bitwise_and_intersects_flags() {
        let a = LineBits::HAS_HYPERLINK | LineBits::RTL | LineBits::BIDI_ENABLED;
        let b = LineBits::RTL | LineBits::DOUBLE_WIDTH;
        let c = a & b;
        assert!(c.contains(LineBits::RTL));
        assert!(!c.contains(LineBits::HAS_HYPERLINK));
        assert!(!c.contains(LineBits::DOUBLE_WIDTH));
    }

    #[test]
    fn flag_insert_and_remove() {
        let mut bits = LineBits::NONE;
        bits.insert(LineBits::HAS_HYPERLINK);
        assert!(bits.contains(LineBits::HAS_HYPERLINK));
        bits.remove(LineBits::HAS_HYPERLINK);
        assert!(!bits.contains(LineBits::HAS_HYPERLINK));
        assert!(bits.is_empty());
    }

    #[test]
    fn flag_toggle() {
        let mut bits = LineBits::NONE;
        bits.toggle(LineBits::BIDI_ENABLED);
        assert!(bits.contains(LineBits::BIDI_ENABLED));
        bits.toggle(LineBits::BIDI_ENABLED);
        assert!(!bits.contains(LineBits::BIDI_ENABLED));
    }

    #[test]
    fn clone_and_eq() {
        let bits = LineBits::HAS_HYPERLINK | LineBits::DOUBLE_WIDTH;
        let cloned = bits;
        assert_eq!(bits, cloned);
    }

    #[test]
    fn debug_format() {
        let bits = LineBits::RTL | LineBits::BIDI_ENABLED;
        let dbg = format!("{:?}", bits);
        assert!(dbg.contains("RTL"));
        assert!(dbg.contains("BIDI_ENABLED"));
    }

    #[test]
    fn bidi_combination() {
        let bidi_rtl = LineBits::BIDI_ENABLED | LineBits::RTL | LineBits::AUTO_DETECT_DIRECTION;
        assert!(bidi_rtl.contains(LineBits::BIDI_ENABLED));
        assert!(bidi_rtl.contains(LineBits::RTL));
        assert!(bidi_rtl.contains(LineBits::AUTO_DETECT_DIRECTION));
    }

    #[test]
    fn hyperlink_scanning_flags() {
        let mut bits = LineBits::NONE;
        // Simulate scanning: set scanned flag, then found flag
        bits.insert(LineBits::SCANNED_IMPLICIT_HYPERLINKS);
        assert!(bits.contains(LineBits::SCANNED_IMPLICIT_HYPERLINKS));
        assert!(!bits.contains(LineBits::HAS_IMPLICIT_HYPERLINKS));

        bits.insert(LineBits::HAS_IMPLICIT_HYPERLINKS);
        assert!(bits.contains(LineBits::SCANNED_IMPLICIT_HYPERLINKS));
        assert!(bits.contains(LineBits::HAS_IMPLICIT_HYPERLINKS));
    }
}
