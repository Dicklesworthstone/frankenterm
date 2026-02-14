#![allow(dead_code)]

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
#[repr(u16)]
pub enum Action {
    None = 0,
    Ignore = 1,
    Print = 2,
    Execute = 3,
    Clear = 4,
    Collect = 5,
    Param = 6,
    EscDispatch = 7,
    CsiDispatch = 8,
    Hook = 9,
    Put = 10,
    Unhook = 11,
    OscStart = 12,
    OscPut = 13,
    OscEnd = 14,
    Utf8 = 15,
    ApcStart = 16,
    ApcPut = 17,
    ApcEnd = 18,
}

impl Action {
    #[inline(always)]
    pub fn from_u16(v: u16) -> Self {
        unsafe { core::mem::transmute(v) }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
#[repr(u16)]
pub enum State {
    Ground = 0,
    Escape = 1,
    EscapeIntermediate = 2,
    CsiEntry = 3,
    CsiParam = 4,
    CsiIntermediate = 5,
    CsiIgnore = 6,
    DcsEntry = 7,
    DcsParam = 8,
    DcsIntermediate = 9,
    DcsPassthrough = 10,
    DcsIgnore = 11,
    OscString = 12,
    SosPmString = 13,
    ApcString = 14,
    // Special states, always last (no tables for these)
    Anywhere = 15,
    Utf8Sequence = 16,
}

impl State {
    #[inline(always)]
    pub fn from_u16(v: u16) -> Self {
        unsafe { core::mem::transmute(v) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Action tests ---

    #[test]
    fn action_discriminant_values() {
        assert_eq!(Action::None as u16, 0);
        assert_eq!(Action::Ignore as u16, 1);
        assert_eq!(Action::Print as u16, 2);
        assert_eq!(Action::Execute as u16, 3);
        assert_eq!(Action::Clear as u16, 4);
        assert_eq!(Action::Collect as u16, 5);
        assert_eq!(Action::Param as u16, 6);
        assert_eq!(Action::EscDispatch as u16, 7);
        assert_eq!(Action::CsiDispatch as u16, 8);
        assert_eq!(Action::Hook as u16, 9);
        assert_eq!(Action::Put as u16, 10);
        assert_eq!(Action::Unhook as u16, 11);
        assert_eq!(Action::OscStart as u16, 12);
        assert_eq!(Action::OscPut as u16, 13);
        assert_eq!(Action::OscEnd as u16, 14);
        assert_eq!(Action::Utf8 as u16, 15);
        assert_eq!(Action::ApcStart as u16, 16);
        assert_eq!(Action::ApcPut as u16, 17);
        assert_eq!(Action::ApcEnd as u16, 18);
    }

    #[test]
    fn action_from_u16_roundtrip() {
        for v in 0..=18u16 {
            let action = Action::from_u16(v);
            assert_eq!(action as u16, v);
        }
    }

    #[test]
    fn action_clone_copy() {
        fn assert_clone<T: Clone>(_: &T) {}

        let a = Action::Print;
        assert_clone(&a);
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn action_debug() {
        let dbg = format!("{:?}", Action::CsiDispatch);
        assert_eq!(dbg, "CsiDispatch");
    }

    #[test]
    fn action_equality() {
        assert_eq!(Action::None, Action::None);
        assert_ne!(Action::None, Action::Print);
        assert_ne!(Action::Execute, Action::Ignore);
    }

    // --- State tests ---

    #[test]
    fn state_discriminant_values() {
        assert_eq!(State::Ground as u16, 0);
        assert_eq!(State::Escape as u16, 1);
        assert_eq!(State::EscapeIntermediate as u16, 2);
        assert_eq!(State::CsiEntry as u16, 3);
        assert_eq!(State::CsiParam as u16, 4);
        assert_eq!(State::CsiIntermediate as u16, 5);
        assert_eq!(State::CsiIgnore as u16, 6);
        assert_eq!(State::DcsEntry as u16, 7);
        assert_eq!(State::DcsParam as u16, 8);
        assert_eq!(State::DcsIntermediate as u16, 9);
        assert_eq!(State::DcsPassthrough as u16, 10);
        assert_eq!(State::DcsIgnore as u16, 11);
        assert_eq!(State::OscString as u16, 12);
        assert_eq!(State::SosPmString as u16, 13);
        assert_eq!(State::ApcString as u16, 14);
        assert_eq!(State::Anywhere as u16, 15);
        assert_eq!(State::Utf8Sequence as u16, 16);
    }

    #[test]
    fn state_from_u16_roundtrip() {
        for v in 0..=16u16 {
            let state = State::from_u16(v);
            assert_eq!(state as u16, v);
        }
    }

    #[test]
    fn state_clone_copy() {
        fn assert_clone<T: Clone>(_: &T) {}

        let a = State::CsiEntry;
        assert_clone(&a);
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn state_debug() {
        let dbg = format!("{:?}", State::OscString);
        assert_eq!(dbg, "OscString");
    }

    #[test]
    fn state_equality() {
        assert_eq!(State::Ground, State::Ground);
        assert_ne!(State::Ground, State::Escape);
        assert_ne!(State::CsiEntry, State::CsiParam);
    }

    #[test]
    fn state_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(State::Ground);
        set.insert(State::Ground); // duplicate
        set.insert(State::Escape);
        assert_eq!(set.len(), 2);
    }
}
