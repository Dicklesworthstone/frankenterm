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
//!   4. **Reparse stability** — display(parse(input)) is a fixed point
//!   5. **Action::append_to coalescence** — Print chars merge correctly

use frankenterm_escape_parser::parser::Parser;
use frankenterm_escape_parser::{Action, DeviceControlMode, Esc, EscCode};
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
///
/// The broad variant covers every byte in the 0x40..0x7e CSI final range
/// so `parse_never_panics_on_csi` exercises unknown finals too.
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

/// Canonical CSI byte strings that the parser is expected to reduce to
/// exactly one `Action::CSI(...)` (ft-mdaon).
///
/// This is deliberately narrower than "all complete CSI sequences": the
/// parser intentionally expands some syntactically complete inputs, such
/// as multi-parameter SGR (`ESC[1;3m`), into multiple actions. The roundtrip
/// MR needs a singleton corpus so it can hard-fail on shape drift instead
/// of silently skipping or asserting a false contract.
fn arb_singleton_csi_sequence() -> impl Strategy<Value = Vec<u8>> {
    let canonical_singletons: Vec<Vec<u8>> = vec![
        b"\x1b[0m".to_vec(),
        b"\x1b[1m".to_vec(),
        b"\x1b[22m".to_vec(),
        b"\x1b[3m".to_vec(),
        b"\x1b[23m".to_vec(),
        b"\x1b[4m".to_vec(),
        b"\x1b[24m".to_vec(),
        b"\x1b[7m".to_vec(),
        b"\x1b[27m".to_vec(),
        b"\x1b[8m".to_vec(),
        b"\x1b[28m".to_vec(),
        b"\x1b[9m".to_vec(),
        b"\x1b[29m".to_vec(),
        b"\x1b[53m".to_vec(),
        b"\x1b[55m".to_vec(),
        b"\x1b[A".to_vec(),
        b"\x1b[5B".to_vec(),
        b"\x1b[10G".to_vec(),
        b"\x1b[2J".to_vec(),
        b"\x1b[3K".to_vec(),
        b"\x1b[!p".to_vec(),
    ];

    proptest::sample::select(canonical_singletons)
}

/// Structured OSC sequences: ESC ] <num> ; <data> ST.
fn arb_osc_sequence() -> impl Strategy<Value = Vec<u8>> {
    (0u16..200u16, "[a-zA-Z0-9 _/.-]{0,32}").prop_map(|(num, data)| {
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

fn parse_and_render(bytes: &[u8]) -> String {
    let mut parser = Parser::new();
    let actions = parser.parse_as_vec(bytes);
    render_canonical_actions(&actions)
}

fn parse_chunked(bytes: &[u8], chunk_sizes: &[usize]) -> Vec<Action> {
    let mut parser = Parser::new();
    let mut actions = Vec::new();

    if chunk_sizes.is_empty() {
        parser.parse(bytes, |action| actions.push(action));
        return actions;
    }

    let mut offset = 0usize;
    let mut chunks = chunk_sizes.iter().copied().cycle();
    while offset < bytes.len() {
        let chunk_len = chunks
            .next()
            .unwrap_or(bytes.len())
            .min(bytes.len() - offset);
        parser.parse(&bytes[offset..offset + chunk_len], |action| {
            actions.push(action)
        });
        offset += chunk_len;
    }

    actions
}

fn render_canonical_actions(actions: &[Action]) -> String {
    let mut rendered = String::new();
    let mut skip_next_st = false;

    for action in actions {
        if skip_next_st && matches!(action, Action::Esc(Esc::Code(EscCode::StringTerminator))) {
            skip_next_st = false;
            continue;
        }

        if matches!(action, Action::DeviceControl(DeviceControlMode::Exit)) {
            rendered.push_str("\x1b\\");
            skip_next_st = true;
            continue;
        }

        let action_needs_canonical_st = matches!(
            action,
            Action::DeviceControl(DeviceControlMode::ShortDeviceControl(_))
                | Action::KittyImage(_)
                | Action::Sixel(_)
                | Action::XtGetTcap(_)
        );
        skip_next_st =
            matches!(action, Action::OperatingSystemCommand(_)) || action_needs_canonical_st;
        rendered.push_str(&action.to_string());
        if action_needs_canonical_st {
            rendered.push_str("\x1b\\");
        }
    }

    rendered
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

    /// PTY reads may split escape sequences at any byte boundary. Parsing a
    /// mixed stream all at once must produce the same actions as feeding the
    /// same bytes through a long-lived parser in generated chunk sizes.
    #[test]
    fn mixed_stream_chunk_boundaries_match_bulk_parse(
        bytes in arb_mixed_terminal_stream(),
        chunk_sizes in proptest::collection::vec(1usize..=32, 1..64),
    ) {
        let mut bulk_parser = Parser::new();
        let bulk_actions = bulk_parser.parse_as_vec(&bytes);
        let chunked_actions = parse_chunked(&bytes, &chunk_sizes);

        prop_assert_eq!(
            chunked_actions,
            bulk_actions,
            "PTY-style chunking changed parsed escape actions for {} bytes and chunks {:?}",
            bytes.len(),
            chunk_sizes
        );
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

    /// Display(parse(input)) should be a parse/render fixed point for
    /// arbitrary bytes. The first pass canonicalizes malformed, truncated,
    /// or otherwise non-displayable input; the second pass must render to
    /// the same canonical byte stream.
    #[test]
    fn parse_render_reparse_is_fixed_point_for_arbitrary_bytes(bytes in arb_bytes()) {
        let rendered_once = parse_and_render(&bytes);
        let rendered_twice = parse_and_render(rendered_once.as_bytes());

        prop_assert_eq!(
            rendered_twice,
            rendered_once,
            "parse/render/reparse must be stable for arbitrary bytes"
        );
    }

    /// Display(parse(input)) should be a parse/render fixed point for
    /// terminal-like mixed streams as well as pure arbitrary noise.
    #[test]
    fn parse_render_reparse_is_fixed_point_for_mixed_streams(
        bytes in arb_mixed_terminal_stream()
    ) {
        let rendered_once = parse_and_render(&bytes);
        let rendered_twice = parse_and_render(rendered_once.as_bytes());

        prop_assert_eq!(
            rendered_twice,
            rendered_once,
            "parse/render/reparse must be stable for mixed terminal streams"
        );
    }

    /// For well-known CSI sequences, parse → display → re-parse must yield
    /// exactly one action on BOTH passes, and those actions must share a
    /// Debug representation (metamorphic roundtrip).
    ///
    /// ft-mdaon: previously the body was an `if actions1.len() == 1 { if
    /// actions2.len() == 1 { ... } }` that silently passed on non-singleton
    /// parses, so any regression that started emitting zero or multiple
    /// actions for a canonical singleton CSI would hide behind the skip.
    /// The generator is now narrowed to `arb_singleton_csi_sequence`, and
    /// both singleton parses are now hard asserts.
    #[test]
    fn csi_roundtrip_via_display(bytes in arb_singleton_csi_sequence()) {
        let mut parser1 = Parser::new();
        let actions1 = parser1.parse_as_vec(&bytes);

        prop_assert!(
            actions1.len() == 1,
            "well-known CSI input {:?} should parse to exactly one action, got {} ({:?})",
            bytes,
            actions1.len(),
            actions1
        );

        let re_encoded = actions1[0].to_string();
        let mut parser2 = Parser::new();
        let actions2 = parser2.parse_as_vec(re_encoded.as_bytes());

        prop_assert!(
            actions2.len() == 1,
            "re-encoded CSI {:?} should round-trip to exactly one action, got {} ({:?})",
            re_encoded,
            actions2.len(),
            actions2
        );

        let repr1 = format!("{:?}", actions1[0]);
        let repr2 = format!("{:?}", actions2[0]);
        prop_assert_eq!(repr1, repr2, "CSI roundtrip Debug representation mismatch");
    }

    // ── Concatenation consistency ───────────────────────────────────

    /// Parsing `a ++ b` on a fresh parser must produce the same rendered
    /// text as feeding `a` then `b` to a single stateful parser.
    ///
    /// ft-mdaon: the suite header advertised this metamorphic relation
    /// (property 3) but had no corresponding test. The relation only
    /// holds when neither `a` nor `b` splits a multi-byte sequence at
    /// its boundary — we restrict inputs to pure printable ASCII so the
    /// parser never has pending intermediate state across the split.
    #[test]
    fn concatenation_consistency_for_printable(
        a in proptest::collection::vec(0x20u8..0x7eu8, 0..64),
        b in proptest::collection::vec(0x20u8..0x7eu8, 0..64),
    ) {
        let mut concat: Vec<u8> = Vec::with_capacity(a.len() + b.len());
        concat.extend_from_slice(&a);
        concat.extend_from_slice(&b);

        let mut bulk_parser = Parser::new();
        let bulk_actions = bulk_parser.parse_as_vec(&concat);
        let bulk_text: String = bulk_actions.iter().map(|act| act.to_string()).collect();

        let mut streaming_parser = Parser::new();
        let mut streaming_actions = streaming_parser.parse_as_vec(&a);
        streaming_actions.extend(streaming_parser.parse_as_vec(&b));
        let streaming_text: String =
            streaming_actions.iter().map(|act| act.to_string()).collect();

        prop_assert_eq!(
            bulk_text,
            streaming_text,
            "parse(a ++ b) must render the same text as parse(a) then parse(b) for printable inputs"
        );
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
