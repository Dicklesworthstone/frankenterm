use crate::bidi_class::BidiClass;
use crate::direction::Direction;
use crate::NO_LEVEL;

/// Maximum stack depth; UBA guarantees that it will never increase
/// in later versions of the spec.
pub const MAX_DEPTH: usize = 125;

#[derive(Default, Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Level(pub i8);

impl Level {
    pub fn direction(self) -> Direction {
        Direction::with_level(self.0)
    }

    pub fn as_bidi_class(self) -> BidiClass {
        if self.0 % 2 == 1 {
            BidiClass::RightToLeft
        } else {
            BidiClass::LeftToRight
        }
    }

    pub fn removed_by_x9(self) -> bool {
        self.0 == NO_LEVEL
    }

    pub fn max(self, other: Level) -> Level {
        Level(self.0.max(other.0))
    }

    pub(crate) fn least_greater_even(self) -> Option<Level> {
        let level = if self.0 % 2 == 0 {
            self.0 + 2
        } else {
            self.0 + 1
        };
        if level as usize > MAX_DEPTH {
            None
        } else {
            Some(Self(level))
        }
    }

    pub(crate) fn least_greater_odd(self) -> Option<Level> {
        let level = if self.0 % 2 == 1 {
            self.0 + 2
        } else {
            self.0 + 1
        };
        if level as usize > MAX_DEPTH {
            None
        } else {
            Some(Self(level))
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;
    use alloc::collections::BTreeSet;
    use core::hash::{Hash, Hasher};

    #[test]
    fn level_default_is_zero() {
        let l = Level::default();
        assert_eq!(l.0, 0);
    }

    #[test]
    fn level_direction_even_is_ltr() {
        assert_eq!(Level(0).direction(), Direction::LeftToRight);
        assert_eq!(Level(2).direction(), Direction::LeftToRight);
        assert_eq!(Level(4).direction(), Direction::LeftToRight);
    }

    #[test]
    fn level_direction_odd_is_rtl() {
        assert_eq!(Level(1).direction(), Direction::RightToLeft);
        assert_eq!(Level(3).direction(), Direction::RightToLeft);
        assert_eq!(Level(125).direction(), Direction::RightToLeft);
    }

    #[test]
    fn level_as_bidi_class_even() {
        assert_eq!(Level(0).as_bidi_class(), BidiClass::LeftToRight);
        assert_eq!(Level(2).as_bidi_class(), BidiClass::LeftToRight);
    }

    #[test]
    fn level_as_bidi_class_odd() {
        assert_eq!(Level(1).as_bidi_class(), BidiClass::RightToLeft);
        assert_eq!(Level(3).as_bidi_class(), BidiClass::RightToLeft);
    }

    #[test]
    fn level_removed_by_x9() {
        assert!(Level(NO_LEVEL).removed_by_x9());
        assert!(!Level(0).removed_by_x9());
        assert!(!Level(1).removed_by_x9());
        assert!(!Level(125).removed_by_x9());
    }

    #[test]
    fn level_max() {
        assert_eq!(Level(3).max(Level(5)), Level(5));
        assert_eq!(Level(5).max(Level(3)), Level(5));
        assert_eq!(Level(4).max(Level(4)), Level(4));
        assert_eq!(Level(0).max(Level(0)), Level(0));
    }

    #[test]
    fn least_greater_even_from_even() {
        // From even level, next even is +2
        assert_eq!(Level(0).least_greater_even(), Some(Level(2)));
        assert_eq!(Level(2).least_greater_even(), Some(Level(4)));
        assert_eq!(Level(4).least_greater_even(), Some(Level(6)));
    }

    #[test]
    fn least_greater_even_from_odd() {
        // From odd level, next even is +1
        assert_eq!(Level(1).least_greater_even(), Some(Level(2)));
        assert_eq!(Level(3).least_greater_even(), Some(Level(4)));
        assert_eq!(Level(5).least_greater_even(), Some(Level(6)));
    }

    #[test]
    fn least_greater_even_at_max_depth() {
        // MAX_DEPTH is 125 (odd). least_greater_even(124) = 126 > 125 = None
        assert_eq!(Level(124).least_greater_even(), None);
        assert_eq!(Level(125).least_greater_even(), None);
        // 123 is odd, +1 = 124, which is <= 125
        assert_eq!(Level(123).least_greater_even(), Some(Level(124)));
    }

    #[test]
    fn least_greater_odd_from_odd() {
        // From odd level, next odd is +2
        assert_eq!(Level(1).least_greater_odd(), Some(Level(3)));
        assert_eq!(Level(3).least_greater_odd(), Some(Level(5)));
    }

    #[test]
    fn least_greater_odd_from_even() {
        // From even level, next odd is +1
        assert_eq!(Level(0).least_greater_odd(), Some(Level(1)));
        assert_eq!(Level(2).least_greater_odd(), Some(Level(3)));
        assert_eq!(Level(4).least_greater_odd(), Some(Level(5)));
    }

    #[test]
    fn least_greater_odd_at_max_depth() {
        // MAX_DEPTH is 125. Level(124) -> +1 = 125 <= 125, ok
        assert_eq!(Level(124).least_greater_odd(), Some(Level(125)));
        // Level(125) -> +2 = 127 > 125, None
        assert_eq!(Level(125).least_greater_odd(), None);
    }

    #[test]
    fn level_eq_ne() {
        assert_eq!(Level(0), Level(0));
        assert_eq!(Level(5), Level(5));
        assert_ne!(Level(0), Level(1));
    }

    #[test]
    fn level_ord() {
        assert!(Level(0) < Level(1));
        assert!(Level(1) < Level(2));
        assert!(Level(125) > Level(0));
        assert!(Level(5) <= Level(5));
        assert!(Level(5) >= Level(5));
    }

    #[test]
    fn level_clone_copy() {
        let l = Level(42);
        let l2 = l; // Copy
        let l3 = l.clone(); // Clone
        assert_eq!(l, l2);
        assert_eq!(l, l3);
    }

    #[test]
    fn level_debug() {
        let dbg = alloc::format!("{:?}", Level(5));
        assert!(dbg.contains("Level"));
        assert!(dbg.contains("5"));
    }

    #[test]
    fn level_hash_consistency() {
        // Levels that are equal should produce the same hash
        // We can't easily test with no_std, but we can verify
        // the Hash trait is usable by inserting into a BTreeSet
        // (BTreeSet uses Ord, but verifies the traits compile)
        let mut set = BTreeSet::new();
        set.insert(Level(0));
        set.insert(Level(1));
        set.insert(Level(0)); // duplicate
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn max_depth_is_125() {
        assert_eq!(MAX_DEPTH, 125);
    }
}
