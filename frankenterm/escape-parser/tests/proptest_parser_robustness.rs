//! Property-based robustness tests for the escape-sequence parser.
//!
//! These tests verify critical safety and correctness properties of
//! `Parser::parse` and `Parser::parse_as_vec` when fed arbitrary,
//! adversarial, and structured byte streams:
//!
//!   1. **No panics** on arbitrary input (crash-freedom)
//!   2. **Determinism** — same input always yields same output
//!   3. **Concatenation consistency** — parse(a++b) == parse(a) ++ parse(b)
//!      for byte-aligned boundaries (modulo incomplete sequences)
//!   4. **Reparse stability** — display(parse(input)) re-parses without panic
//!   5. **Action::append_to coalescence** — Print chars merge correctly

use frankenterm_escape_parser::parser::Parser;
use frankenterm_escape_parser::Action;
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────

/// Arbitrary bytes — the broadest input space.
fn arb_bytes() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..512)
}

/// Bytes biased toward printable ASCII (the common case).
fn arb_printable_bytes() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(0x20u8..0x7eu8, 0..256)
}

/// Structured CSI sequences: ESC [ <params> <final byte>.
fn arb_csi_sequence() -> impl Strategy<Value = Vec<u8>> {
    (
        proptest::collection::vec(0u16..1000u16, 0..5),
        0x40u8..0x7eu8, // final byte
    )
        .prop_map(|(params, final_byte)| {
            let mut bytes = vec![0x1b, b'['];
            for (i, p) in params.iter().enumerate() {
                if i > 0 {
                    bytes.push(b';');
                }
                bytes.extend_from_slice(p.to_string().as_bytes());
            }
            bytes.push(final_byte);
            bytes
        })
}

/// Structured OSC sequences: ESC ] <num> ; <data> ST.
fn arb_osc_sequence() -> impl Strategy<Value = Vec<u8>> {
    (0u16..200u16, "[a-zA-Z0-9 _/.-]{0,32}")
        .prop_map(|(num, data)| {
            let mut bytes = vec![0x1b, b']'];
            bytes.extend_from_slice(num.to_string().as_bytes());
            bytes.push(b';');
            bytes.extend_from_slice(data.as_bytes());
            bytes.push(0x1b);
            bytes.push(b'\\'); // ST = ESC backslash
            bytes
        })
}

/// Mix of structured sequences and random noise.
fn arb_mixed_terminal_stream() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(
        prop_oneof![
            // Printable ASCII run
            proptest::collection::vec(0x20u8..0x7eu8, 1..32),
            // C0 control codes
            proptest::collection::vec(0u8..0x20u8, 1..4),
            // CSI sequence
            arb_csi_sequence(),
            // OSC sequence
            arb_osc_sequence(),
            // Escape + single byte
            (0x40u8..0x5fu8).prop_map(|b| vec![0x1b, b]),
            // Random bytes
            proptest::collection::vec(any::<u8>(), 1..16),
        ],
        1..8,
    )
    .prop_map(|segments| segments.into_iter().flatten().collect())
}

// ── Tests ───────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    // ── Crash-freedom ───────────────────────────────────────────────

    /// Parser must never panic on arbitrary byte sequences.
    #[test]
    fn parse_never_panics_on_arbitrary_bytes(bytes in arb_bytes()) {
        let mut parser = Parser::new();
        let _actions = parser.parse_as_vec(&bytes);
        // If we get here, the parser survived.
    }

    /// Parser must never panic on printable ASCII.
    #[test]
    fn parse_never_panics_on_printable_ascii(bytes in arb_printable_bytes()) {
        let mut parser = Parser::new();
        let actions = parser.parse_as_vec(&bytes);
        // All printable bytes should produce Print/PrintString actions.
        for action in &actions {
            let is_print = matches!(action, Action::Print(_) | Action::PrintString(_));
            prop_assert!(is_print, "printable ASCII should yield Print actions, got: {action}");
        }
    }

    /// Parser must never panic on structured CSI sequences.
    #[test]
    fn parse_never_panics_on_csi(bytes in arb_csi_sequence()) {
        let mut parser = Parser::new();
        let actions = parser.parse_as_vec(&bytes);
        // CSI should produce exactly one CSI action (or Esc + Print for unknown).
        prop_assert!(!actions.is_empty(), "CSI sequence should produce at least one action");
    }

    /// Parser must never panic on structured OSC sequences.
    #[test]
    fn parse_never_panics_on_osc(bytes in arb_osc_sequence()) {
        let mut parser = Parser::new();
        let _actions = parser.parse_as_vec(&bytes);
    }

    /// Parser must never panic on mixed streams.
    #[test]
    fn parse_never_panics_on_mixed_stream(bytes in arb_mixed_terminal_stream()) {
        let mut parser = Parser::new();
        let _actions = parser.parse_as_vec(&bytes);
    }

    // ── Determinism ─────────────────────────────────────────────────

    /// Same input → same output, always.
    #[test]
    fn parse_is_deterministic(bytes in arb_bytes()) {
        let mut parser1 = Parser::new();
        let mut parser2 = Parser::new();
        let actions1 = parser1.parse_as_vec(&bytes);
        let actions2 = parser2.parse_as_vec(&bytes);
        let repr1: Vec<String> = actions1.iter().map(|a| format!("{a:?}")).collect();
        let repr2: Vec<String> = actions2.iter().map(|a| format!("{a:?}")).collect();
        prop_assert_eq!(repr1, repr2, "parse should be deterministic");
    }

    // ── Reparse stability ───────────────────────────────────────────

    /// Display(parse(input)) should re-parse without panic.
    /// This is a weaker form of roundtrip — we don't require equality,
    /// just that the re-encoded output is itself parseable.
    #[test]
    fn display_then_reparse_never_panics(bytes in arb_mixed_terminal_stream()) {
        let mut parser = Parser::new();
        let actions = parser.parse_as_vec(&bytes);

        // Re-encode actions to string via Display.
        let re_encoded: String = actions.iter().map(|a| a.to_string()).collect();

        // Re-parse the display output — should not panic.
        let mut parser2 = Parser::new();
        let _actions2 = parser2.parse_as_vec(re_encoded.as_bytes());
    }

    /// For pure CSI sequences, parse → display → re-parse should yield
    /// an action with the same Debug representation (metamorphic roundtrip).
    #[test]
    fn csi_roundtrip_via_display(bytes in arb_csi_sequence()) {
        let mut parser1 = Parser::new();
        let actions1 = parser1.parse_as_vec(&bytes);

        if actions1.len() == 1 {
            let re_encoded = actions1[0].to_string();
            let mut parser2 = Parser::new();
            let actions2 = parser2.parse_as_vec(re_encoded.as_bytes());

            if actions2.len() == 1 {
                let repr1 = format!("{:?}", actions1[0]);
                let repr2 = format!("{:?}", actions2[0]);
                prop_assert_eq!(repr1, repr2, "CSI roundtrip mismatch");
            }
        }
    }

    // ── Action coalescence ──────────────────────────────────────────

    /// append_to must correctly merge consecutive Print chars into PrintString.
    #[test]
    fn append_to_coalesces_prints(chars in proptest::collection::vec(any::<char>(), 2..32)) {
        let mut dest: Vec<Action> = Vec::new();
        for &c in &chars {
            Action::Print(c).append_to(&mut dest);
        }

        // Should have coalesced into one or more PrintString actions.
        if chars.len() >= 2 {
            prop_assert!(
                dest.len() < chars.len(),
                "append_to should coalesce: {} actions from {} chars",
                dest.len(),
                chars.len()
            );
        }

        // Collect all characters from the coalesced result.
        let mut result_chars = String::new();
        for action in &dest {
            match action {
                Action::Print(c) => result_chars.push(*c),
                Action::PrintString(s) => result_chars.push_str(s),
                _ => prop_assert!(false, "unexpected action type: {action:?}"),
            }
        }

        let expected: String = chars.iter().collect();
        prop_assert_eq!(result_chars, expected, "coalesced chars must match input");
    }

    // ── Incremental parsing ─────────────────────────────────────────

    /// Parsing byte-by-byte should produce equivalent output to parsing
    /// the entire buffer at once, when the input consists solely of
    /// complete printable characters (no incomplete multibyte or escape
    /// sequences split across boundaries).
    #[test]
    fn single_byte_ascii_same_as_bulk(bytes in proptest::collection::vec(0x20u8..0x7eu8, 1..64)) {
        // Bulk parse.
        let mut bulk_parser = Parser::new();
        let bulk_actions = bulk_parser.parse_as_vec(&bytes);

        // Byte-by-byte parse.
        let mut incr_parser = Parser::new();
        let mut incr_actions: Vec<Action> = Vec::new();
        for &b in &bytes {
            for action in incr_parser.parse_as_vec(&[b]) {
                action.append_to(&mut incr_actions);
            }
        }

        // Both should produce the same printable content.
        let bulk_text: String = bulk_actions.iter().map(|a| a.to_string()).collect();
        let incr_text: String = incr_actions.iter().map(|a| a.to_string()).collect();
        prop_assert_eq!(bulk_text, incr_text, "incremental vs bulk mismatch");
    }
}
