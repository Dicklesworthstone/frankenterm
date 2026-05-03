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

fn parse_chunked(bytes: &[u8], chunk_sizes: &[usize]) -> Vec<Action> {
    let mut actions = Vec::new();
    let mut parser = Parser::new();
    let mut offset = 0;

    for chunk_size in chunk_sizes {
        if offset >= bytes.len() {
            break;
        }

        let end = offset
            .saturating_add(*chunk_size)
            .max(offset + 1)
            .min(bytes.len());
        parser.parse(&bytes[offset..end], |action| actions.push(action));
        offset = end;
    }

    if offset < bytes.len() {
        parser.parse(&bytes[offset..], |action| actions.push(action));
    }

    actions
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

fn arb_csi_introducer() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![Just(b"\x1b[".to_vec()), Just(vec![0x9b])]
}

fn arb_csi_param_or_intermediate_byte() -> impl Strategy<Value = u8> {
    prop_oneof![
        0x30u8..=0x3f, // numeric/private params and separators
        0x20u8..=0x2f, // intermediates
        Just(0x07u8),  // C0 controls execute inside CSI states
        Just(0x08u8),
        Just(0x09u8),
        Just(0x0du8),
        Just(0x7fu8), // ignored DEL
    ]
}

fn arb_nested_csi_bytes() -> impl Strategy<Value = Vec<u8>> {
    (
        arb_csi_introducer(),
        proptest::collection::vec(arb_csi_param_or_intermediate_byte(), 0..16),
        proptest::collection::vec(
            (
                arb_csi_introducer(),
                proptest::collection::vec(arb_csi_param_or_intermediate_byte(), 0..16),
            ),
            1..8,
        ),
        prop::option::of(prop::sample::select(vec![
            b'm', b'H', b'J', b'K', b'S', b'T',
        ])),
    )
        .prop_map(|(first, first_body, nested, final_byte)| {
            let mut bytes = first;
            bytes.extend(first_body);
            for (intro, body) in nested {
                bytes.extend(intro);
                bytes.extend(body);
            }
            if let Some(final_byte) = final_byte {
                bytes.push(final_byte);
            }
            bytes
        })
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

    #[test]
    fn valid_action_stream_parse_is_chunk_boundary_invariant(
        actions in proptest::collection::vec(arb_roundtrippable_action(), 0..64),
        chunk_sizes in proptest::collection::vec(1usize..=32, 0..32),
    ) {
        let encoded = serialize_actions(&actions);
        let expected = parse_as_vec(encoded.as_bytes());
        prop_assert_eq!(&expected, &actions);

        let mut parser = Parser::new();
        let mut actual = Vec::new();
        let mut offset = 0;
        let bytes = encoded.as_bytes();

        for chunk_size in chunk_sizes {
            if offset >= bytes.len() {
                break;
            }

            let end = offset.saturating_add(chunk_size).min(bytes.len());
            parser.parse(&bytes[offset..end], |action| actual.push(action));
            offset = end;
        }

        if offset < bytes.len() {
            parser.parse(&bytes[offset..], |action| actual.push(action));
        }

        prop_assert_eq!(actual, expected);
    }

    #[test]
    fn nested_csi_sequences_recover_to_following_csi_action(
        mut nested in arb_nested_csi_bytes(),
        chunk_sizes in proptest::collection::vec(1usize..=8, 0..32),
    ) {
        let expected_suffix = Action::CSI(CSI::Cursor(Cursor::Position {
            line: OneBased::new(13),
            col: OneBased::new(17),
        }));
        nested.extend_from_slice(b"\x1b[13;17H");

        let actions = parse_as_vec(&nested);
        let action_count = actions.len();
        prop_assert_eq!(parse_count(&nested), action_count);
        prop_assert_eq!(actions.last(), Some(&expected_suffix));

        let ceiling = nested
            .len()
            .saturating_mul(MAX_ACTIONS_PER_BYTE)
            .saturating_add(8);
        prop_assert!(
            action_count <= ceiling,
            "nested CSI parser emitted {} actions for {} input bytes, above ceiling {}",
            action_count,
            nested.len(),
            ceiling,
        );

        prop_assert_eq!(parse_chunked(&nested, &chunk_sizes), actions);
    }
}
