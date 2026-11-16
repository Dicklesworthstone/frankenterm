// Builds up the transition table for a state machine based on
// https://vt100.net/emu/dec_ansi_parser

use crate::enums::{Action, State};

/// Apply all u8 values to `fn(u8) -> u16`, return `[u16; 256]`.
macro_rules! define_table {
    ( $func:tt ) => {{
        const fn gen() -> [u16; 256] {
            let mut arr = [0; 256];

            let mut i = 0;
            while i < 256 {
                arr[i] = $func(i as u8);
                i += 1;
            }
            return arr;
        }
        gen()
    }};
}

const fn pack(action: Action, state: State) -> u16 {
    ((action as u16) << 8) | (state as u16)
}

const fn anywhere_or(i: u8, state: State) -> u16 {
    use Action::*;
    use State::*;
    match i {
        0x18 => pack(Execute, Ground),
        0x1a => pack(Execute, Ground),
        0x80..=0x8f => pack(Execute, Ground),
        0x91..=0x97 => pack(Execute, Ground),
        0x99 => pack(Execute, Ground),
        0x9a => pack(Execute, Ground),
        0x9c => pack(None, Ground),
        0x1b => pack(None, Escape),
        0x98 => pack(None, SosPmString),
        0x9e => pack(None, SosPmString),
        0x9f => pack(None, SosPmString),
        0x90 => pack(None, DcsEntry),
        0x9d => pack(None, OscString),
        0x9b => pack(None, CsiEntry),
        _ => pack(None, state),
    }
}

const fn ground(i: u8) -> u16 {
    use Action::*;
    use State::*;
    match i {
        0x00..=0x17 => pack(Execute, Ground),
        0x19 => pack(Execute, Ground),
        0x1c..=0x1f => pack(Execute, Ground),
        0x20..=0x7f => pack(Print, Ground),
        // The following three ranges allow for
        // UTF-8 multibyte sequences to be recognized
        // and emitted as byte sequences in the ground
        // state.
        0xc2..=0xdf => pack(Utf8, Utf8Sequence),
        0xe0..=0xef => pack(Utf8, Utf8Sequence),
        0xf0..=0xf4 => pack(Utf8, Utf8Sequence),
        _ => anywhere_or(i, Ground),
    }
}

const fn escape(i: u8) -> u16 {
    use Action::*;
    use State::*;
    match i {
        0x00..=0x17 => pack(Execute, Escape),
        0x19 => pack(Execute, Escape),
        0x1c..=0x1f => pack(Execute, Escape),
        0x7f => pack(Ignore, Escape),
        0x20..=0x2f => pack(Collect, EscapeIntermediate),
        0x30..=0x4f => pack(EscDispatch, Ground),
        0x51..=0x57 => pack(EscDispatch, Ground),
        0x59 => pack(EscDispatch, Ground),
        0x5a => pack(EscDispatch, Ground),
        0x5c => pack(EscDispatch, Ground),
        0x60..=0x7e => pack(EscDispatch, Ground),
        0x5b => pack(None, CsiEntry),
        0x5d => pack(None, OscString),
        0x50 => pack(None, DcsEntry),
        0x58 => pack(None, SosPmString),
        0x5e => pack(None, SosPmString),
        0x5f => pack(None, ApcString),
        _ => anywhere_or(i, Escape),
    }
}

const fn escape_intermediate(i: u8) -> u16 {
    use Action::*;
    use State::*;
    match i {
        0x00..=0x17 => pack(Execute, EscapeIntermediate),
        0x19 => pack(Execute, EscapeIntermediate),
        0x1c..=0x1f => pack(Execute, EscapeIntermediate),
        0x20..=0x2f => pack(Collect, EscapeIntermediate),
        0x7f => pack(Ignore, EscapeIntermediate),
        0x30..=0x7e => pack(EscDispatch, Ground),
        _ => anywhere_or(i, EscapeIntermediate),
    }
}

const fn csi_entry(i: u8) -> u16 {
    use Action::*;
    use State::*;
    match i {
        0x00..=0x17 => pack(Execute, CsiEntry),
        0x19 => pack(Execute, CsiEntry),
        0x1c..=0x1f => pack(Execute, CsiEntry),
        0x7f => pack(Ignore, CsiEntry),
        0x20..=0x2f => pack(Collect, CsiIntermediate),
        0x3a => pack(None, CsiIgnore),
        0x30..=0x39 => pack(Param, CsiParam),
        0x3b => pack(Param, CsiParam),
        0x3c..=0x3f => pack(Collect, CsiParam),
        0x40..=0x7e => pack(CsiDispatch, Ground),
        _ => anywhere_or(i, CsiEntry),
    }
}

const fn csi_param(i: u8) -> u16 {
    use Action::*;
    use State::*;
    match i {
        0x00..=0x17 => pack(Execute, CsiParam),
        0x19 => pack(Execute, CsiParam),
        0x1c..=0x1f => pack(Execute, CsiParam),
        0x30..=0x3b => pack(Param, CsiParam),
        0x7f => pack(Ignore, CsiParam),
        0x3c..=0x3f => pack(None, CsiIgnore),
        0x20..=0x2f => pack(Collect, CsiIntermediate),
        0x40..=0x7e => pack(CsiDispatch, Ground),
        _ => anywhere_or(i, CsiParam),
    }
}

const fn csi_intermediate(i: u8) -> u16 {
    use Action::*;
    use State::*;
    match i {
        0x00..=0x17 => pack(Execute, CsiIntermediate),
        0x19 => pack(Execute, CsiIntermediate),
        0x1c..=0x1f => pack(Execute, CsiIntermediate),
        0x20..=0x2f => pack(Collect, CsiIntermediate),
        0x7f => pack(Ignore, CsiIntermediate),
        0x30..=0x3f => pack(None, CsiIgnore),
        0x40..=0x7e => pack(CsiDispatch, Ground),
        _ => anywhere_or(i, CsiIntermediate),
    }
}

const fn csi_ignore(i: u8) -> u16 {
    use Action::*;
    use State::*;
    match i {
        0x00..=0x17 => pack(Execute, CsiIgnore),
        0x19 => pack(Execute, CsiIgnore),
        0x1c..=0x1f => pack(Execute, CsiIgnore),
        0x20..=0x3f => pack(Ignore, CsiIgnore),
        0x7f => pack(Ignore, CsiIgnore),
        0x40..=0x7e => pack(None, Ground),
        _ => anywhere_or(i, CsiIgnore),
    }
}

const fn dcs_entry(i: u8) -> u16 {
    use Action::*;
    use State::*;
    match i {
        0x00..=0x17 => pack(Ignore, DcsEntry),
        0x19 => pack(Ignore, DcsEntry),
        0x1c..=0x1f => pack(Ignore, DcsEntry),
        0x7f => pack(Ignore, DcsEntry),
        0x3a => pack(None, DcsIgnore),
        0x20..=0x2f => pack(Collect, DcsIntermediate),
        0x30..=0x39 => pack(Param, DcsParam),
        0x3b => pack(Param, DcsParam),
        0x3c..=0x3f => pack(Collect, DcsParam),
        0x40..=0x7e => pack(None, DcsPassthrough),
        _ => anywhere_or(i, DcsEntry),
    }
}

const fn dcs_param(i: u8) -> u16 {
    use Action::*;
    use State::*;
    match i {
        0x00..=0x17 => pack(Ignore, DcsParam),
        0x19 => pack(Ignore, DcsParam),
        0x1c..=0x1f => pack(Ignore, DcsParam),
        0x30..=0x39 => pack(Param, DcsParam),
        0x3b => pack(Param, DcsParam),
        0x7f => pack(Ignore, DcsParam),
        0x3a => pack(None, DcsIgnore),
        0x3c..=0x3f => pack(None, DcsIgnore),
        0x20..=0x2f => pack(Collect, DcsIntermediate),
        0x40..=0x7e => pack(None, DcsPassthrough),
        _ => anywhere_or(i, DcsParam),
    }
}

const fn dcs_intermediate(i: u8) -> u16 {
    use Action::*;
    use State::*;
    match i {
        0x00..=0x17 => pack(Ignore, DcsIntermediate),
        0x19 => pack(Ignore, DcsIntermediate),
        0x1c..=0x1f => pack(Ignore, DcsIntermediate),
        0x20..=0x2f => pack(Collect, DcsIntermediate),
        0x7f => pack(Ignore, DcsIntermediate),
        0x30..=0x3f => pack(None, DcsIgnore),
        0x40..=0x7e => pack(None, DcsPassthrough),
        _ => anywhere_or(i, DcsIntermediate),
    }
}

const fn dcs_passthrough(i: u8) -> u16 {
    use Action::*;
    use State::*;
    match i {
        0x00..=0x17 => pack(Put, DcsPassthrough),
        0x19 => pack(Put, DcsPassthrough),
        0x1c..=0x1f => pack(Put, DcsPassthrough),
        0x20..=0x7e => pack(Put, DcsPassthrough),
        0x7f => pack(Ignore, DcsPassthrough),
        _ => anywhere_or(i, DcsPassthrough),
    }
}

const fn dcs_ignore(i: u8) -> u16 {
    use Action::*;
    use State::*;
    match i {
        0x00..=0x17 => pack(Ignore, DcsIgnore),
        0x19 => pack(Ignore, DcsIgnore),
        0x1c..=0x1f => pack(Ignore, DcsIgnore),
        0x20..=0x7f => pack(Ignore, DcsIgnore),
        _ => anywhere_or(i, DcsIgnore),
    }
}

const fn osc_string(i: u8) -> u16 {
    use Action::*;
    use State::*;
    match i {
        0x00..=0x06 => pack(Ignore, OscString),
        // Using BEL in place of ST is a deviation from
        // https://vt100.net/emu/dec_ansi_parser and was
        // introduced AFAICT by xterm
        0x07 => pack(Ignore, Ground),
        0x08..=0x17 => pack(Ignore, OscString),
        0x19 => pack(Ignore, OscString),
        0x1c..=0x1f => pack(Ignore, OscString),
        0x20..=0x7f => pack(OscPut, OscString),
        // This extended range allows for UTF-8 characters
        // to be embedded in OSC parameters.  It is not
        // part of the base state machine.
        0xc2..=0xdf => pack(Utf8, Utf8Sequence),
        0xe0..=0xef => pack(Utf8, Utf8Sequence),
        0xf0..=0xf4 => pack(Utf8, Utf8Sequence),
        _ => anywhere_or(i, OscString),
    }
}

const fn sos_pm_string(i: u8) -> u16 {
    use Action::*;
    use State::*;
    match i {
        0x00..=0x17 => pack(Ignore, SosPmString),
        0x19 => pack(Ignore, SosPmString),
        0x1c..=0x1f => pack(Ignore, SosPmString),
        0x20..=0x7f => pack(Ignore, SosPmString),
        _ => anywhere_or(i, SosPmString),
    }
}

const fn apc_string(i: u8) -> u16 {
    use Action::*;
    use State::*;
    match i {
        0x00..=0x17 => pack(ApcPut, ApcString),
        0x19 => pack(ApcPut, ApcString),
        0x1c..=0x1f => pack(ApcPut, ApcString),
        0x20..=0x7f => pack(ApcPut, ApcString),
        _ => anywhere_or(i, ApcString),
    }
}

pub(crate) static TRANSITIONS: [[u16; 256]; 15] = [
    define_table!(ground),
    define_table!(escape),
    define_table!(escape_intermediate),
    define_table!(csi_entry),
    define_table!(csi_param),
    define_table!(csi_intermediate),
    define_table!(csi_ignore),
    define_table!(dcs_entry),
    define_table!(dcs_param),
    define_table!(dcs_intermediate),
    define_table!(dcs_passthrough),
    define_table!(dcs_ignore),
    define_table!(osc_string),
    define_table!(sos_pm_string),
    define_table!(apc_string),
];

pub(crate) static ENTRY: [Action; 17] = [
    Action::None,     // Ground
    Action::Clear,    // Escape
    Action::None,     // EscapeIntermediate
    Action::Clear,    // CsiEntry
    Action::None,     // CsiParam
    Action::None,     // CsiIntermediate
    Action::None,     // CsiIgnore
    Action::Clear,    // DcsEntry
    Action::None,     // DcsParam
    Action::None,     // DcsIntermediate
    Action::Hook,     // DcsPassthrough
    Action::None,     // DcsIgnore
    Action::OscStart, // OscString
    Action::None,     // SosPmString
    Action::ApcStart, // ApcString
    Action::None,     // Anywhere
    Action::None,     // Utf8Sequence
];

pub(crate) static EXIT: [Action; 17] = [
    Action::None,   // Ground
    Action::None,   // Escape
    Action::None,   // EscapeIntermediate
    Action::None,   // CsiEntry
    Action::None,   // CsiParam
    Action::None,   // CsiIntermediate
    Action::None,   // CsiIgnore
    Action::None,   // DcsEntry
    Action::None,   // DcsParam
    Action::None,   // DcsIntermediate
    Action::Unhook, // DcsPassthrough
    Action::None,   // DcsIgnore
    Action::OscEnd, // OscString
    Action::None,   // SosPmString
    Action::ApcEnd, // ApcString
    Action::None,   // Anywhere
    Action::None,   // Utf8Sequence
];

#[cfg(test)]
mod tests {
    use super::*;

    fn unpack(v: u16) -> (Action, State) {
        (Action::from_u16(v >> 8), State::from_u16(v & 0xff))
    }

    fn lookup(state: State, byte: u8) -> (Action, State) {
        unpack(TRANSITIONS[state as usize][byte as usize])
    }

    #[test]
    fn test_transitions() {
        let v = format!("{:?}", TRANSITIONS).as_bytes().to_vec();
        assert_eq!(
            (
                v.len(),
                hash(&v, 0, 1),
                hash(&v, 5381, 33), // djb2
                hash(&v, 0, 65599), // sdbm
            ),
            (17385, 799944, 12647816782590382477, 3641575052870461598)
        );
    }

    fn hash(v: &[u8], init: u64, mul: u64) -> u64 {
        v.iter()
            .fold(init, |a, &b| a.wrapping_mul(mul).wrapping_add(b as u64))
    }

    // --- Table dimension tests ---

    #[test]
    fn transitions_table_has_15_states() {
        assert_eq!(TRANSITIONS.len(), 15);
    }

    #[test]
    fn transitions_table_each_state_has_256_entries() {
        for (i, table) in TRANSITIONS.iter().enumerate() {
            assert_eq!(table.len(), 256, "state index {i} has wrong entry count");
        }
    }

    #[test]
    fn entry_table_has_17_entries() {
        assert_eq!(ENTRY.len(), 17);
    }

    #[test]
    fn exit_table_has_17_entries() {
        assert_eq!(EXIT.len(), 17);
    }

    // --- Entry actions ---

    #[test]
    fn entry_action_escape_is_clear() {
        assert_eq!(ENTRY[State::Escape as usize], Action::Clear);
    }

    #[test]
    fn entry_action_csi_entry_is_clear() {
        assert_eq!(ENTRY[State::CsiEntry as usize], Action::Clear);
    }

    #[test]
    fn entry_action_dcs_entry_is_clear() {
        assert_eq!(ENTRY[State::DcsEntry as usize], Action::Clear);
    }

    #[test]
    fn entry_action_osc_string_is_osc_start() {
        assert_eq!(ENTRY[State::OscString as usize], Action::OscStart);
    }

    #[test]
    fn entry_action_dcs_passthrough_is_hook() {
        assert_eq!(ENTRY[State::DcsPassthrough as usize], Action::Hook);
    }

    #[test]
    fn entry_action_apc_string_is_apc_start() {
        assert_eq!(ENTRY[State::ApcString as usize], Action::ApcStart);
    }

    #[test]
    fn entry_action_ground_is_none() {
        assert_eq!(ENTRY[State::Ground as usize], Action::None);
    }

    // --- Exit actions ---

    #[test]
    fn exit_action_dcs_passthrough_is_unhook() {
        assert_eq!(EXIT[State::DcsPassthrough as usize], Action::Unhook);
    }

    #[test]
    fn exit_action_osc_string_is_osc_end() {
        assert_eq!(EXIT[State::OscString as usize], Action::OscEnd);
    }

    #[test]
    fn exit_action_apc_string_is_apc_end() {
        assert_eq!(EXIT[State::ApcString as usize], Action::ApcEnd);
    }

    #[test]
    fn exit_action_ground_is_none() {
        assert_eq!(EXIT[State::Ground as usize], Action::None);
    }

    #[test]
    fn exit_action_escape_is_none() {
        assert_eq!(EXIT[State::Escape as usize], Action::None);
    }

    // --- Ground state transitions ---

    #[test]
    fn ground_printable_chars_produce_print() {
        for b in 0x20..=0x7fu8 {
            let (action, state) = lookup(State::Ground, b);
            assert_eq!(
                (action, state),
                (Action::Print, State::Ground),
                "byte 0x{b:02x}"
            );
        }
    }

    #[test]
    fn ground_c0_controls_execute() {
        // 0x00-0x17 (except 0x18, 0x1a which are "anywhere")
        for b in 0x00..=0x17u8 {
            let (action, state) = lookup(State::Ground, b);
            assert_eq!(
                (action, state),
                (Action::Execute, State::Ground),
                "byte 0x{b:02x}"
            );
        }
        // 0x19
        assert_eq!(
            lookup(State::Ground, 0x19),
            (Action::Execute, State::Ground)
        );
        // 0x1c-0x1f
        for b in 0x1c..=0x1fu8 {
            let (action, state) = lookup(State::Ground, b);
            assert_eq!(
                (action, state),
                (Action::Execute, State::Ground),
                "byte 0x{b:02x}"
            );
        }
    }

    #[test]
    fn ground_esc_transitions_to_escape() {
        assert_eq!(lookup(State::Ground, 0x1b), (Action::None, State::Escape));
    }

    #[test]
    fn ground_utf8_two_byte_lead() {
        // 0xc2-0xdf → Utf8 + Utf8Sequence
        for b in 0xc2..=0xdfu8 {
            let (action, state) = lookup(State::Ground, b);
            assert_eq!(
                (action, state),
                (Action::Utf8, State::Utf8Sequence),
                "byte 0x{b:02x}"
            );
        }
    }

    #[test]
    fn ground_utf8_three_byte_lead() {
        for b in 0xe0..=0xefu8 {
            assert_eq!(
                lookup(State::Ground, b),
                (Action::Utf8, State::Utf8Sequence),
                "byte 0x{b:02x}"
            );
        }
    }

    #[test]
    fn ground_utf8_four_byte_lead() {
        for b in 0xf0..=0xf4u8 {
            assert_eq!(
                lookup(State::Ground, b),
                (Action::Utf8, State::Utf8Sequence),
                "byte 0x{b:02x}"
            );
        }
    }

    // --- Anywhere transitions (from ground) ---

    #[test]
    fn ground_can_cancel_0x18() {
        assert_eq!(
            lookup(State::Ground, 0x18),
            (Action::Execute, State::Ground)
        );
    }

    #[test]
    fn ground_can_cancel_0x1a() {
        assert_eq!(
            lookup(State::Ground, 0x1a),
            (Action::Execute, State::Ground)
        );
    }

    #[test]
    fn ground_c1_0x9b_enters_csi() {
        assert_eq!(lookup(State::Ground, 0x9b), (Action::None, State::CsiEntry));
    }

    #[test]
    fn ground_c1_0x9d_enters_osc() {
        assert_eq!(
            lookup(State::Ground, 0x9d),
            (Action::None, State::OscString)
        );
    }

    #[test]
    fn ground_c1_0x90_enters_dcs() {
        assert_eq!(lookup(State::Ground, 0x90), (Action::None, State::DcsEntry));
    }

    #[test]
    fn ground_c1_0x9c_returns_ground() {
        assert_eq!(lookup(State::Ground, 0x9c), (Action::None, State::Ground));
    }

    // --- Escape state transitions ---

    #[test]
    fn escape_open_bracket_to_csi_entry() {
        assert_eq!(lookup(State::Escape, 0x5b), (Action::None, State::CsiEntry));
    }

    #[test]
    fn escape_close_bracket_to_osc_string() {
        assert_eq!(
            lookup(State::Escape, 0x5d),
            (Action::None, State::OscString)
        );
    }

    #[test]
    fn escape_p_to_dcs_entry() {
        assert_eq!(lookup(State::Escape, 0x50), (Action::None, State::DcsEntry));
    }

    #[test]
    fn escape_underscore_to_apc_string() {
        assert_eq!(
            lookup(State::Escape, 0x5f),
            (Action::None, State::ApcString)
        );
    }

    #[test]
    fn escape_final_bytes_dispatch_to_ground() {
        // 0x30-0x4f → EscDispatch + Ground
        for b in 0x30..=0x4fu8 {
            let (action, state) = lookup(State::Escape, b);
            assert_eq!(
                (action, state),
                (Action::EscDispatch, State::Ground),
                "byte 0x{b:02x}"
            );
        }
        // 0x60-0x7e → EscDispatch + Ground
        for b in 0x60..=0x7eu8 {
            let (action, state) = lookup(State::Escape, b);
            assert_eq!(
                (action, state),
                (Action::EscDispatch, State::Ground),
                "byte 0x{b:02x}"
            );
        }
    }

    #[test]
    fn escape_intermediates_collect() {
        for b in 0x20..=0x2fu8 {
            let (action, state) = lookup(State::Escape, b);
            assert_eq!(
                (action, state),
                (Action::Collect, State::EscapeIntermediate),
                "byte 0x{b:02x}"
            );
        }
    }

    #[test]
    fn escape_del_is_ignored() {
        assert_eq!(lookup(State::Escape, 0x7f), (Action::Ignore, State::Escape));
    }

    // --- CSI Entry state transitions ---

    #[test]
    fn csi_entry_digits_to_param() {
        for b in 0x30..=0x39u8 {
            let (action, state) = lookup(State::CsiEntry, b);
            assert_eq!(
                (action, state),
                (Action::Param, State::CsiParam),
                "byte 0x{b:02x}"
            );
        }
    }

    #[test]
    fn csi_entry_semicolon_to_param() {
        assert_eq!(
            lookup(State::CsiEntry, 0x3b),
            (Action::Param, State::CsiParam)
        );
    }

    #[test]
    fn csi_entry_colon_to_ignore() {
        assert_eq!(
            lookup(State::CsiEntry, 0x3a),
            (Action::None, State::CsiIgnore)
        );
    }

    #[test]
    fn csi_entry_private_markers_collect() {
        // 0x3c-0x3f: <, =, >, ?
        for b in 0x3c..=0x3fu8 {
            let (action, state) = lookup(State::CsiEntry, b);
            assert_eq!(
                (action, state),
                (Action::Collect, State::CsiParam),
                "byte 0x{b:02x}"
            );
        }
    }

    #[test]
    fn csi_entry_final_bytes_dispatch() {
        for b in 0x40..=0x7eu8 {
            let (action, state) = lookup(State::CsiEntry, b);
            assert_eq!(
                (action, state),
                (Action::CsiDispatch, State::Ground),
                "byte 0x{b:02x}"
            );
        }
    }

    // --- CSI Param state transitions ---

    #[test]
    fn csi_param_digits_stay() {
        for b in 0x30..=0x3bu8 {
            let (action, state) = lookup(State::CsiParam, b);
            assert_eq!(
                (action, state),
                (Action::Param, State::CsiParam),
                "byte 0x{b:02x}"
            );
        }
    }

    #[test]
    fn csi_param_final_dispatch() {
        for b in 0x40..=0x7eu8 {
            assert_eq!(
                lookup(State::CsiParam, b),
                (Action::CsiDispatch, State::Ground),
                "byte 0x{b:02x}"
            );
        }
    }

    #[test]
    fn csi_param_intermediate_collect() {
        for b in 0x20..=0x2fu8 {
            assert_eq!(
                lookup(State::CsiParam, b),
                (Action::Collect, State::CsiIntermediate),
                "byte 0x{b:02x}"
            );
        }
    }

    // --- OSC String state transitions ---

    #[test]
    fn osc_string_bel_returns_ground() {
        assert_eq!(
            lookup(State::OscString, 0x07),
            (Action::Ignore, State::Ground)
        );
    }

    #[test]
    fn osc_string_printable_osc_put() {
        for b in 0x20..=0x7fu8 {
            assert_eq!(
                lookup(State::OscString, b),
                (Action::OscPut, State::OscString),
                "byte 0x{b:02x}"
            );
        }
    }

    #[test]
    fn osc_string_esc_transitions_to_escape() {
        assert_eq!(
            lookup(State::OscString, 0x1b),
            (Action::None, State::Escape)
        );
    }

    #[test]
    fn osc_string_utf8_lead_bytes() {
        for b in 0xc2..=0xdfu8 {
            assert_eq!(
                lookup(State::OscString, b),
                (Action::Utf8, State::Utf8Sequence),
                "byte 0x{b:02x}"
            );
        }
    }

    // --- DCS state transitions ---

    #[test]
    fn dcs_entry_final_to_passthrough() {
        for b in 0x40..=0x7eu8 {
            assert_eq!(
                lookup(State::DcsEntry, b),
                (Action::None, State::DcsPassthrough),
                "byte 0x{b:02x}"
            );
        }
    }

    #[test]
    fn dcs_passthrough_data_put() {
        for b in 0x20..=0x7eu8 {
            assert_eq!(
                lookup(State::DcsPassthrough, b),
                (Action::Put, State::DcsPassthrough),
                "byte 0x{b:02x}"
            );
        }
    }

    #[test]
    fn dcs_passthrough_del_ignored() {
        assert_eq!(
            lookup(State::DcsPassthrough, 0x7f),
            (Action::Ignore, State::DcsPassthrough)
        );
    }

    // --- APC String state transitions ---

    #[test]
    fn apc_string_data_apc_put() {
        for b in 0x20..=0x7fu8 {
            assert_eq!(
                lookup(State::ApcString, b),
                (Action::ApcPut, State::ApcString),
                "byte 0x{b:02x}"
            );
        }
    }

    #[test]
    fn apc_string_esc_transitions_to_escape() {
        assert_eq!(
            lookup(State::ApcString, 0x1b),
            (Action::None, State::Escape)
        );
    }

    // --- Pack/unpack roundtrip ---

    #[test]
    fn pack_unpack_roundtrip() {
        let actions = [
            Action::None,
            Action::Print,
            Action::Execute,
            Action::CsiDispatch,
            Action::EscDispatch,
        ];
        let states = [
            State::Ground,
            State::Escape,
            State::CsiEntry,
            State::OscString,
        ];
        for &action in &actions {
            for &state in &states {
                let packed = pack(action, state);
                let (a, s) = unpack(packed);
                assert_eq!((a, s), (action, state));
            }
        }
    }

    // --- SOS/PM String ---

    #[test]
    fn sos_pm_string_ignores_printable() {
        for b in 0x20..=0x7fu8 {
            assert_eq!(
                lookup(State::SosPmString, b),
                (Action::Ignore, State::SosPmString),
                "byte 0x{b:02x}"
            );
        }
    }

    #[test]
    fn sos_pm_string_esc_to_escape() {
        assert_eq!(
            lookup(State::SosPmString, 0x1b),
            (Action::None, State::Escape)
        );
    }
}
