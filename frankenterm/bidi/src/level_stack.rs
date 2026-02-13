use crate::bidi_class::BidiClass;
use crate::level::{Level, MAX_DEPTH};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum Override {
    Neutral,
    LTR,
    RTL,
}

/// An implementation of the stack/STATUSSTACKELEMENT from bidiref
#[derive(Debug)]
pub(crate) struct LevelStack {
    embedding_level: [Level; MAX_DEPTH],
    override_status: [Override; MAX_DEPTH],
    isolate_status: [bool; MAX_DEPTH],
    /// Current index into the stack arrays above
    depth: usize,
}

impl LevelStack {
    pub fn new() -> Self {
        Self {
            embedding_level: [Level::default(); MAX_DEPTH],
            override_status: [Override::Neutral; MAX_DEPTH],
            isolate_status: [false; MAX_DEPTH],
            depth: 0,
        }
    }

    pub fn depth(&self) -> usize {
        self.depth
    }

    pub fn push(&mut self, level: Level, override_status: Override, isolate_status: bool) {
        let depth = self.depth;
        if depth >= MAX_DEPTH {
            return;
        }
        log::trace!(
            "pushing level={:?} override={:?} isolate={} at depth={}",
            level,
            override_status,
            isolate_status,
            depth
        );
        self.embedding_level[depth] = level;
        self.override_status[depth] = override_status;
        self.isolate_status[depth] = isolate_status;
        self.depth += 1;
    }

    pub fn pop(&mut self) {
        if self.depth > 0 {
            self.depth -= 1;
        }
    }

    pub fn embedding_level(&self) -> Level {
        self.embedding_level[self.depth - 1]
    }

    pub fn override_status(&self) -> Override {
        self.override_status[self.depth - 1]
    }

    pub fn apply_override(&self, bc: &mut BidiClass) {
        match self.override_status() {
            Override::LTR => *bc = BidiClass::LeftToRight,
            Override::RTL => *bc = BidiClass::RightToLeft,
            Override::Neutral => {}
        }
    }

    pub fn isolate_status(&self) -> bool {
        self.isolate_status[self.depth - 1]
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;

    #[test]
    fn new_stack_has_zero_depth() {
        let stack = LevelStack::new();
        assert_eq!(stack.depth(), 0);
    }

    #[test]
    fn push_increments_depth() {
        let mut stack = LevelStack::new();
        stack.push(Level(0), Override::Neutral, false);
        assert_eq!(stack.depth(), 1);
        stack.push(Level(1), Override::LTR, true);
        assert_eq!(stack.depth(), 2);
    }

    #[test]
    fn pop_decrements_depth() {
        let mut stack = LevelStack::new();
        stack.push(Level(0), Override::Neutral, false);
        stack.push(Level(1), Override::RTL, true);
        assert_eq!(stack.depth(), 2);
        stack.pop();
        assert_eq!(stack.depth(), 1);
        stack.pop();
        assert_eq!(stack.depth(), 0);
    }

    #[test]
    fn pop_at_zero_depth_is_noop() {
        let mut stack = LevelStack::new();
        stack.pop(); // should not panic or underflow
        assert_eq!(stack.depth(), 0);
        stack.pop();
        assert_eq!(stack.depth(), 0);
    }

    #[test]
    fn embedding_level_returns_top_of_stack() {
        let mut stack = LevelStack::new();
        stack.push(Level(0), Override::Neutral, false);
        assert_eq!(stack.embedding_level(), Level(0));
        stack.push(Level(5), Override::LTR, false);
        assert_eq!(stack.embedding_level(), Level(5));
        stack.pop();
        assert_eq!(stack.embedding_level(), Level(0));
    }

    #[test]
    fn override_status_returns_top_of_stack() {
        let mut stack = LevelStack::new();
        stack.push(Level(0), Override::LTR, false);
        assert_eq!(stack.override_status(), Override::LTR);
        stack.push(Level(1), Override::RTL, false);
        assert_eq!(stack.override_status(), Override::RTL);
        stack.push(Level(2), Override::Neutral, false);
        assert_eq!(stack.override_status(), Override::Neutral);
    }

    #[test]
    fn isolate_status_returns_top_of_stack() {
        let mut stack = LevelStack::new();
        stack.push(Level(0), Override::Neutral, true);
        assert!(stack.isolate_status());
        stack.push(Level(1), Override::Neutral, false);
        assert!(!stack.isolate_status());
        stack.pop();
        assert!(stack.isolate_status());
    }

    #[test]
    fn apply_override_ltr() {
        let mut stack = LevelStack::new();
        stack.push(Level(0), Override::LTR, false);
        let mut bc = BidiClass::RightToLeft;
        stack.apply_override(&mut bc);
        assert_eq!(bc, BidiClass::LeftToRight);
    }

    #[test]
    fn apply_override_rtl() {
        let mut stack = LevelStack::new();
        stack.push(Level(0), Override::RTL, false);
        let mut bc = BidiClass::LeftToRight;
        stack.apply_override(&mut bc);
        assert_eq!(bc, BidiClass::RightToLeft);
    }

    #[test]
    fn apply_override_neutral_preserves() {
        let mut stack = LevelStack::new();
        stack.push(Level(0), Override::Neutral, false);
        let mut bc = BidiClass::ArabicNumber;
        stack.apply_override(&mut bc);
        assert_eq!(bc, BidiClass::ArabicNumber);
    }

    #[test]
    fn push_at_max_depth_is_noop() {
        let mut stack = LevelStack::new();
        for i in 0..MAX_DEPTH {
            stack.push(Level(i as i8), Override::Neutral, false);
        }
        assert_eq!(stack.depth(), MAX_DEPTH);
        // Push beyond MAX_DEPTH should be silently ignored
        stack.push(Level(126), Override::LTR, true);
        assert_eq!(stack.depth(), MAX_DEPTH);
        // Top should still be the last valid push
        assert_eq!(stack.embedding_level(), Level(124));
    }

    #[test]
    fn push_pop_many() {
        let mut stack = LevelStack::new();
        for i in 0..10 {
            stack.push(Level(i), Override::Neutral, i % 2 == 0);
        }
        assert_eq!(stack.depth(), 10);
        assert_eq!(stack.embedding_level(), Level(9));
        assert!(!stack.isolate_status()); // 9 is odd

        for _ in 0..10 {
            stack.pop();
        }
        assert_eq!(stack.depth(), 0);
    }

    #[test]
    fn override_enum_equality() {
        assert_eq!(Override::Neutral, Override::Neutral);
        assert_eq!(Override::LTR, Override::LTR);
        assert_eq!(Override::RTL, Override::RTL);
        assert_ne!(Override::LTR, Override::RTL);
        assert_ne!(Override::LTR, Override::Neutral);
        assert_ne!(Override::RTL, Override::Neutral);
    }

    #[test]
    fn override_clone_copy() {
        let o = Override::LTR;
        let o2 = o; // Copy
        let o3 = o.clone(); // Clone
        assert_eq!(o, o2);
        assert_eq!(o, o3);
    }

    #[test]
    fn override_debug() {
        let dbg = alloc::format!("{:?}", Override::LTR);
        assert!(dbg.contains("LTR"));
        let dbg = alloc::format!("{:?}", Override::RTL);
        assert!(dbg.contains("RTL"));
        let dbg = alloc::format!("{:?}", Override::Neutral);
        assert!(dbg.contains("Neutral"));
    }

    #[test]
    fn stack_debug() {
        let stack = LevelStack::new();
        let dbg = alloc::format!("{:?}", stack);
        assert!(dbg.contains("LevelStack"));
    }
}
