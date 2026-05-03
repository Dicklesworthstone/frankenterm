use frankenterm_escape_parser::csi::{Cursor, CSI};
use frankenterm_escape_parser::parser::Parser;
use frankenterm_escape_parser::{Action, ControlCode, OneBased};
use num_traits::FromPrimitive;
use proptest::prelude::*;

const MAX_ACTIONS_PER_BYTE: usize = 8;

fn parse_as_vec(bytes: &[u8]) -> Vec<Action> {
    let mut parser = Parser::new();
    parser.parse_as_vec(bytes)
}

fn parse_count(bytes: &[u8]) -> usize {
    let mut count = 0;
    let mut parser = Parser::new();
    parser.parse(bytes, |_action| count += 1);
    count
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn arbitrary_bytes_parse_without_panic_and_entrypoints_agree(
        bytes in proptest::collection::vec(any::<u8>(), 0..4096),
    ) {
        let actions = parse_as_vec(&bytes);
        prop_assert_eq!(parse_count(&bytes), actions.len());

        let ceiling = bytes
            .len()
            .saturating_mul(MAX_ACTIONS_PER_BYTE)
            .saturating_add(8);
        prop_assert!(
            actions.len() <= ceiling,
            "escape parser emitted {} actions for {} input bytes, above ceiling {}",
            actions.len(),
            bytes.len(),
            ceiling,
        );
    }

    #[test]
    fn cursor_position_csi_roundtrips_to_action(
        line in 1u32..=4096,
        col in 1u32..=4096,
    ) {
        let csi = CSI::Cursor(Cursor::Position {
            line: OneBased::new(line),
            col: OneBased::new(col),
        });
        let encoded = csi.to_string();

        prop_assert_eq!(
            parse_as_vec(encoded.as_bytes()),
            vec![Action::CSI(csi)],
        );
    }

    #[test]
    fn c0_control_bytes_parse_to_control_actions(byte in prop_oneof![
        Just(0_u8),
        1_u8..=0x1a,
        0x1c_u8..=0x1f,
    ]) {
        let expected = ControlCode::from_u8(byte)
            .expect("generated C0 byte should map to a ControlCode");
        prop_assert_eq!(
            parse_as_vec(&[byte]),
            vec![Action::Control(expected)],
        );
    }
}
