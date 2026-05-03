use frankenterm_escape_parser::csi::{Blink, Cursor, Edit, Intensity, Sgr, Underline, CSI};
use frankenterm_escape_parser::esc::{Esc, EscCode};
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

fn arb_one_based() -> impl Strategy<Value = OneBased> {
    (1u32..=4096).prop_map(OneBased::new)
}

fn arb_c0_control_action() -> impl Strategy<Value = Action> {
    prop_oneof![Just(0_u8), 1_u8..=0x1a, 0x1c_u8..=0x1f,].prop_map(|byte| {
        Action::Control(
            ControlCode::from_u8(byte).expect("generated C0 byte should map to a ControlCode"),
        )
    })
}

fn arb_sgr_action() -> impl Strategy<Value = Action> {
    prop_oneof![
        Just(Sgr::Reset),
        prop::sample::select(vec![Intensity::Normal, Intensity::Bold, Intensity::Half])
            .prop_map(Sgr::Intensity),
        prop::sample::select(vec![
            Underline::None,
            Underline::Single,
            Underline::Double,
            Underline::Curly,
            Underline::Dotted,
            Underline::Dashed,
        ])
        .prop_map(Sgr::Underline),
        prop::sample::select(vec![Blink::None, Blink::Slow, Blink::Rapid]).prop_map(Sgr::Blink),
        any::<bool>().prop_map(Sgr::Italic),
        any::<bool>().prop_map(Sgr::Inverse),
        any::<bool>().prop_map(Sgr::Invisible),
        any::<bool>().prop_map(Sgr::StrikeThrough),
    ]
    .prop_map(|sgr| Action::CSI(CSI::Sgr(sgr)))
}

fn arb_cursor_action() -> impl Strategy<Value = Action> {
    prop_oneof![
        (arb_one_based(), arb_one_based())
            .prop_map(|(line, col)| { Action::CSI(CSI::Cursor(Cursor::Position { line, col })) }),
        (1u32..=4096).prop_map(|n| Action::CSI(CSI::Cursor(Cursor::Up(n)))),
        (1u32..=4096).prop_map(|n| Action::CSI(CSI::Cursor(Cursor::Down(n)))),
        (1u32..=4096).prop_map(|n| Action::CSI(CSI::Cursor(Cursor::Left(n)))),
        (1u32..=4096).prop_map(|n| Action::CSI(CSI::Cursor(Cursor::Right(n)))),
        (1u32..=4096).prop_map(|n| Action::CSI(CSI::Cursor(Cursor::NextLine(n)))),
        (1u32..=4096).prop_map(|n| Action::CSI(CSI::Cursor(Cursor::PrecedingLine(n)))),
        arb_one_based().prop_map(|col| Action::CSI(CSI::Cursor(Cursor::CharacterAbsolute(col)))),
    ]
}

fn arb_edit_action() -> impl Strategy<Value = Action> {
    prop_oneof![
        (1u32..=4096).prop_map(|n| Action::CSI(CSI::Edit(Edit::DeleteCharacter(n)))),
        (1u32..=4096).prop_map(|n| Action::CSI(CSI::Edit(Edit::DeleteLine(n)))),
        (1u32..=4096).prop_map(|n| Action::CSI(CSI::Edit(Edit::EraseCharacter(n)))),
        (1u32..=4096).prop_map(|n| Action::CSI(CSI::Edit(Edit::InsertCharacter(n)))),
        (1u32..=4096).prop_map(|n| Action::CSI(CSI::Edit(Edit::InsertLine(n)))),
        (1u32..=4096).prop_map(|n| Action::CSI(CSI::Edit(Edit::ScrollDown(n)))),
        (1u32..=4096).prop_map(|n| Action::CSI(CSI::Edit(Edit::ScrollUp(n)))),
        (1u32..=4096).prop_map(|n| Action::CSI(CSI::Edit(Edit::Repeat(n)))),
    ]
}

fn arb_esc_action() -> impl Strategy<Value = Action> {
    prop::sample::select(vec![
        EscCode::Index,
        EscCode::NextLine,
        EscCode::HorizontalTabSet,
        EscCode::ReverseIndex,
        EscCode::DecSaveCursorPosition,
        EscCode::DecRestoreCursorPosition,
        EscCode::DecLineDrawingG0,
        EscCode::AsciiCharacterSetG0,
        EscCode::DecLineDrawingG1,
        EscCode::AsciiCharacterSetG1,
    ])
    .prop_map(|code| Action::Esc(Esc::Code(code)))
}

fn arb_roundtrippable_action() -> impl Strategy<Value = Action> {
    prop_oneof![
        arb_c0_control_action(),
        arb_sgr_action(),
        arb_cursor_action(),
        arb_edit_action(),
        arb_esc_action(),
    ]
}

fn serialize_actions(actions: &[Action]) -> String {
    actions.iter().map(ToString::to_string).collect()
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

    #[test]
    fn valid_actions_roundtrip_through_display_and_parse(action in arb_roundtrippable_action()) {
        let encoded = action.to_string();
        prop_assert_eq!(parse_as_vec(encoded.as_bytes()), vec![action]);
    }

    #[test]
    fn concatenated_valid_actions_parse_as_the_same_action_stream(
        actions in proptest::collection::vec(arb_roundtrippable_action(), 0..64),
    ) {
        let encoded = serialize_actions(&actions);
        prop_assert_eq!(parse_as_vec(encoded.as_bytes()), actions);
    }
}
