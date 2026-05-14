use proptest::prelude::*;

use frankenterm_core::kitty_keyboard::{
    KeyEvent, KeyEventKind, KittyKbdCsi, KittyKbdFlag, KittyKbdFlagSet, KittyKbdHealth,
    KittyKbdParseError, KittyKbdStack, MAX_STACK_DEPTH, PushOutcome, encode_key_event,
    parse_csi_kbd, render_query_response,
};

#[derive(Debug, Clone)]
enum StackOp {
    Push(u8),
    Pop,
    Query,
}

fn arb_stack_op() -> impl Strategy<Value = StackOp> {
    prop_oneof![
        any::<u8>().prop_map(StackOp::Push),
        Just(StackOp::Pop),
        Just(StackOp::Query),
    ]
}

fn arb_event_kind() -> impl Strategy<Value = KeyEventKind> {
    prop_oneof![
        Just(KeyEventKind::Press),
        Just(KeyEventKind::Repeat),
        Just(KeyEventKind::Release),
    ]
}

fn arb_key_event() -> impl Strategy<Value = KeyEvent> {
    (
        prop_oneof![
            Just(8u32),
            Just(9u32),
            Just(13u32),
            Just(27u32),
            0x20u32..=0x7eu32,
            0x80u32..=0x10ffffu32,
        ],
        any::<u8>(),
        arb_event_kind(),
        prop::option::of(0x20u32..=0x10ffffu32),
        prop::option::of("[ -~]{0,16}"),
    )
        .prop_map(
            |(key, modifiers, event_kind, alternate, associated_text)| KeyEvent {
                key,
                modifiers,
                event_kind,
                alternate,
                associated_text,
            },
        )
}

fn arb_flag_set() -> impl Strategy<Value = KittyKbdFlagSet> {
    any::<u8>().prop_map(KittyKbdFlagSet::from_bits_truncate)
}

fn legacy_byte_for_key(key: u32) -> Option<Vec<u8>> {
    match key {
        8 => Some(b"\x08".to_vec()),
        9 => Some(b"\t".to_vec()),
        13 => Some(b"\r".to_vec()),
        27 => Some(b"\x1b".to_vec()),
        cp if cp >= 0x20 && cp != 0x7f => {
            let mut buf = [0u8; 4];
            Some(
                char::from_u32(cp)
                    .unwrap_or('?')
                    .encode_utf8(&mut buf)
                    .as_bytes()
                    .to_vec(),
            )
        }
        _ => Some(Vec::new()),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_kitty_keyboard_push_parser_truncates_to_protocol_bits(raw in any::<u8>()) {
        let body = format!("> {raw} u");
        let parsed = parse_csi_kbd(&body).expect("u8 push bodies must parse");

        prop_assert_eq!(
            parsed,
            KittyKbdCsi::Push {
                flags: KittyKbdFlagSet::from_bits_truncate(raw),
            },
        );

        if let KittyKbdCsi::Push { flags } = parsed {
            prop_assert_eq!(flags.bits, raw & KittyKbdFlagSet::ALL_BITS);
        }
    }

    #[test]
    fn proptest_kitty_keyboard_rejects_out_of_range_or_payload_bearing_control_bodies(
        too_large in 256u16..=4096,
        payload in "[0-9A-Za-z]{1,8}",
    ) {
        prop_assert_eq!(
            parse_csi_kbd(&format!("> {too_large} u")),
            Err(KittyKbdParseError::MalformedFlags),
        );
        prop_assert_eq!(
            parse_csi_kbd(&format!("< {payload} u")),
            Err(KittyKbdParseError::MalformedFlags),
        );
        prop_assert_eq!(
            parse_csi_kbd(&format!("? {payload} u")),
            Err(KittyKbdParseError::MalformedFlags),
        );
    }

    #[test]
    fn proptest_kitty_keyboard_stack_never_exceeds_depth_and_matches_model(
        ops in prop::collection::vec(arb_stack_op(), 0..512),
    ) {
        let mut stack = KittyKbdStack::new();
        let mut model = Vec::new();
        let mut accepted_pushes = 0u64;
        let mut rejected_pushes = 0u64;
        let mut pops = 0u64;
        let mut max_depth = 0usize;

        for op in ops {
            match op {
                StackOp::Push(bits) => {
                    let flags = KittyKbdFlagSet::from_bits_truncate(bits);
                    if model.len() < MAX_STACK_DEPTH {
                        prop_assert_eq!(stack.push(flags), PushOutcome::Pushed);
                        model.push(flags);
                        accepted_pushes += 1;
                        max_depth = max_depth.max(model.len());
                    } else {
                        prop_assert_eq!(stack.push(flags), PushOutcome::Rejected);
                        rejected_pushes += 1;
                    }
                }
                StackOp::Pop => {
                    stack.pop();
                    model.pop();
                    pops += 1;
                }
                StackOp::Query => {}
            }

            prop_assert!(stack.depth() <= MAX_STACK_DEPTH);
            prop_assert_eq!(stack.depth(), model.len());
            prop_assert_eq!(
                stack.current(),
                model.last().copied().unwrap_or_else(KittyKbdFlagSet::empty),
            );
            prop_assert_eq!(stack.pushes_total, accepted_pushes);
            prop_assert_eq!(stack.pushes_rejected_total, rejected_pushes);
            prop_assert_eq!(stack.pops_total, pops);
            prop_assert_eq!(stack.max_depth_observed, max_depth as u32);
        }

        let health = KittyKbdHealth::from_stack(&stack, &Default::default());
        prop_assert_eq!(health.current_depth, model.len() as u32);
        prop_assert_eq!(health.max_depth_observed, max_depth as u32);
        prop_assert_eq!(health.is_safe(), rejected_pushes == 0);
    }

    #[test]
    fn proptest_kitty_keyboard_query_response_is_csi_u_with_truncated_bits(flags in arb_flag_set()) {
        let rendered = render_query_response(flags);

        prop_assert_eq!(rendered.as_str(), format!("\x1b[?{}u", flags.bits));
        prop_assert!(rendered.as_bytes().starts_with(b"\x1b[?"));
        prop_assert!(rendered.as_bytes().ends_with(b"u"));
        prop_assert!(flags.bits <= KittyKbdFlagSet::ALL_BITS);
    }

    #[test]
    fn proptest_kitty_keyboard_encoding_is_legacy_or_ascii_csi(event in arb_key_event(), flags in arb_flag_set()) {
        let encoded = encode_key_event(&event, flags);

        if event.event_kind == KeyEventKind::Release
            && !flags.contains(KittyKbdFlag::ReportEventTypes)
        {
            prop_assert!(encoded.is_empty());
            return Ok(());
        }

        let csi_forced = flags.contains(KittyKbdFlag::ReportAllKeysAsEscapes)
            || (flags.contains(KittyKbdFlag::Disambiguate) && matches!(event.key, 8 | 9 | 13 | 27))
            || flags.contains(KittyKbdFlag::ReportAlternateKeys)
            || (flags.contains(KittyKbdFlag::ReportAssociatedText)
                && event
                    .associated_text
                    .as_ref()
                    .is_some_and(|text| !text.is_empty()));

        if csi_forced {
            prop_assert!(encoded.starts_with(b"\x1b["));
            prop_assert!(encoded.ends_with(b"u"));
            prop_assert!(encoded.iter().all(u8::is_ascii));
        } else {
            prop_assert_eq!(encoded, legacy_byte_for_key(event.key).unwrap());
        }
    }

    #[test]
    fn proptest_report_associated_text_payload_forces_csi_without_other_flags(
        key in 0x20u32..=0x7eu32,
        text in "[ -~]{1,16}",
    ) {
        let mut flags = KittyKbdFlagSet::empty();
        flags.set(KittyKbdFlag::ReportAssociatedText);
        let event = KeyEvent {
            key,
            modifiers: 0,
            event_kind: KeyEventKind::Press,
            alternate: None,
            associated_text: Some(text.clone()),
        };

        let encoded = encode_key_event(&event, flags);
        let expected_payload = text
            .chars()
            .map(|c| (c as u32).to_string())
            .collect::<Vec<_>>()
            .join(":");
        let expected = format!("\x1b[{key};{expected_payload}u").into_bytes();

        prop_assert_eq!(encoded, expected);
    }

    #[test]
    fn proptest_empty_associated_text_matches_absent_associated_text(
        key in 0x20u32..=0x7eu32,
        all_keys_as_escapes in any::<bool>(),
    ) {
        let mut flags = KittyKbdFlagSet::empty();
        flags.set(KittyKbdFlag::ReportAssociatedText);
        if all_keys_as_escapes {
            flags.set(KittyKbdFlag::ReportAllKeysAsEscapes);
        }

        let absent = KeyEvent {
            key,
            modifiers: 0,
            event_kind: KeyEventKind::Press,
            alternate: None,
            associated_text: None,
        };
        let empty = KeyEvent {
            associated_text: Some(String::new()),
            ..absent.clone()
        };

        prop_assert_eq!(encode_key_event(&empty, flags), encode_key_event(&absent, flags));
    }
}
