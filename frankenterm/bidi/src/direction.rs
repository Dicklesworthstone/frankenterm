use crate::bidi_class::BidiClass;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Direction {
    LeftToRight,
    RightToLeft,
}

impl Direction {
    pub fn with_level(level: i8) -> Self {
        if level % 2 == 1 {
            Self::RightToLeft
        } else {
            Self::LeftToRight
        }
    }

    pub fn opposite(self) -> Self {
        if self == Direction::LeftToRight {
            Direction::RightToLeft
        } else {
            Direction::LeftToRight
        }
    }

    pub fn as_bidi_class(self) -> BidiClass {
        match self {
            Self::RightToLeft => BidiClass::RightToLeft,
            Self::LeftToRight => BidiClass::LeftToRight,
        }
    }

    /// Given a DoubleEndedIterator, returns an iterator that will
    /// either walk it in its natural order if Direction==LeftToRight,
    /// or in reverse order if Direction==RightToLeft
    pub fn iter<I: DoubleEndedIterator<Item = T>, T>(self, iter: I) -> DirectionIter<I, T> {
        DirectionIter::wrap(iter, self)
    }
}

pub enum DirectionIter<I: DoubleEndedIterator<Item = T>, T> {
    LTR(I),
    RTL(core::iter::Rev<I>),
}

impl<I: DoubleEndedIterator<Item = T>, T> DirectionIter<I, T> {
    pub fn wrap(iter: I, direction: Direction) -> Self {
        match direction {
            Direction::LeftToRight => Self::LTR(iter),
            Direction::RightToLeft => Self::RTL(iter.rev()),
        }
    }
}

impl<I: DoubleEndedIterator<Item = T>, T> Iterator for DirectionIter<I, T> {
    type Item = I::Item;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::LTR(i) => i.next(),
            Self::RTL(i) => i.next(),
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    #[test]
    fn with_level_even_is_ltr() {
        assert_eq!(Direction::with_level(0), Direction::LeftToRight);
        assert_eq!(Direction::with_level(2), Direction::LeftToRight);
        assert_eq!(Direction::with_level(4), Direction::LeftToRight);
        assert_eq!(Direction::with_level(124), Direction::LeftToRight);
    }

    #[test]
    fn with_level_odd_is_rtl() {
        assert_eq!(Direction::with_level(1), Direction::RightToLeft);
        assert_eq!(Direction::with_level(3), Direction::RightToLeft);
        assert_eq!(Direction::with_level(5), Direction::RightToLeft);
        assert_eq!(Direction::with_level(125), Direction::RightToLeft);
    }

    #[test]
    fn with_level_negative() {
        // -1 is odd (NO_LEVEL), should be RTL
        assert_eq!(Direction::with_level(-1), Direction::RightToLeft);
        // -2 is even
        assert_eq!(Direction::with_level(-2), Direction::LeftToRight);
    }

    #[test]
    fn opposite_ltr_is_rtl() {
        assert_eq!(Direction::LeftToRight.opposite(), Direction::RightToLeft);
    }

    #[test]
    fn opposite_rtl_is_ltr() {
        assert_eq!(Direction::RightToLeft.opposite(), Direction::LeftToRight);
    }

    #[test]
    fn opposite_is_involution() {
        assert_eq!(
            Direction::LeftToRight.opposite().opposite(),
            Direction::LeftToRight
        );
        assert_eq!(
            Direction::RightToLeft.opposite().opposite(),
            Direction::RightToLeft
        );
    }

    #[test]
    fn as_bidi_class_ltr() {
        assert_eq!(
            Direction::LeftToRight.as_bidi_class(),
            BidiClass::LeftToRight
        );
    }

    #[test]
    fn as_bidi_class_rtl() {
        assert_eq!(
            Direction::RightToLeft.as_bidi_class(),
            BidiClass::RightToLeft
        );
    }

    #[test]
    fn direction_eq_and_ne() {
        assert_eq!(Direction::LeftToRight, Direction::LeftToRight);
        assert_eq!(Direction::RightToLeft, Direction::RightToLeft);
        assert_ne!(Direction::LeftToRight, Direction::RightToLeft);
    }

    #[test]
    fn direction_clone_copy() {
        let d = Direction::LeftToRight;
        let d2 = d; // Copy
        let d3 = d.clone(); // Clone
        assert_eq!(d, d2);
        assert_eq!(d, d3);
    }

    #[test]
    fn direction_debug() {
        let dbg = alloc::format!("{:?}", Direction::LeftToRight);
        assert!(dbg.contains("LeftToRight"));
        let dbg = alloc::format!("{:?}", Direction::RightToLeft);
        assert!(dbg.contains("RightToLeft"));
    }

    #[test]
    fn direction_iter_ltr_preserves_order() {
        let items: Vec<i32> = Direction::LeftToRight
            .iter(vec![1, 2, 3].into_iter())
            .collect();
        assert_eq!(items, vec![1, 2, 3]);
    }

    #[test]
    fn direction_iter_rtl_reverses_order() {
        let items: Vec<i32> = Direction::RightToLeft
            .iter(vec![1, 2, 3].into_iter())
            .collect();
        assert_eq!(items, vec![3, 2, 1]);
    }

    #[test]
    fn direction_iter_empty() {
        let items: Vec<i32> = Direction::LeftToRight
            .iter(Vec::<i32>::new().into_iter())
            .collect();
        assert!(items.is_empty());
        let items: Vec<i32> = Direction::RightToLeft
            .iter(Vec::<i32>::new().into_iter())
            .collect();
        assert!(items.is_empty());
    }

    #[test]
    fn direction_iter_single_element() {
        let items: Vec<i32> = Direction::LeftToRight.iter(vec![42].into_iter()).collect();
        assert_eq!(items, vec![42]);
        let items: Vec<i32> = Direction::RightToLeft.iter(vec![42].into_iter()).collect();
        assert_eq!(items, vec![42]);
    }

    #[test]
    fn direction_iter_wrap_ltr() {
        let iter = DirectionIter::wrap(vec![10, 20, 30].into_iter(), Direction::LeftToRight);
        let items: Vec<i32> = iter.collect();
        assert_eq!(items, vec![10, 20, 30]);
    }

    #[test]
    fn direction_iter_wrap_rtl() {
        let iter = DirectionIter::wrap(vec![10, 20, 30].into_iter(), Direction::RightToLeft);
        let items: Vec<i32> = iter.collect();
        assert_eq!(items, vec![30, 20, 10]);
    }
}
