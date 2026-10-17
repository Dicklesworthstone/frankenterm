//! Equivalence gate for the CSI fast path (ft-yccm0.3.2.4).
//!
//! With print batching on, the fast path scans complete CSI sequences at
//! ground straight from the bytes, decodes SGR and the common finals
//! directly, answers repeated SGRs from a one-entry cache, and decodes a lone
//! codepoint in place. With it off, every one of those bytes goes through the
//! state machine and every CSI through `CSI::parse`. The action stream must be
//! identical either way, for the same chunking, which is what this checks on
//! random streams mixing complete sequences, CSI fragments, text, controls,
//! malformed UTF-8 and overflowing parameters.

use frankenterm_escape_parser::Action;
use frankenterm_escape_parser::parser::Parser;
use proptest::prelude::*;

fn parse_chunks(chunks: &[&[u8]], csi_fast_path: bool) -> Vec<Action> {
    let mut parser = Parser::new();
    parser.set_print_batching(true);
    parser.set_csi_fast_path(csi_fast_path);
    assert_eq!(parser.csi_fast_path(), csi_fast_path);
    let mut actions = Vec::new();
    for chunk in chunks {
        parser.parse(chunk, |action| actions.push(action));
    }
    actions
}

fn assert_same_stream(chunks: &[&[u8]]) {
    assert_eq!(
        parse_chunks(chunks, true),
        parse_chunks(chunks, false),
        "chunks {:?}",
        chunks
    );
}

/// Complete sequences: the operator's SGRs, the other common finals, colon
/// sub-parameters, and sequences the fast path leaves alone.
const SEQUENCES: &[&str] = &[
    "\x1b[38;5;196m",
    "\x1b[48;5;21m",
    "\x1b[58;5;9m",
    "\x1b[38;2;1;2;3m",
    "\x1b[48;2;255;0;128m",
    "\x1b[0m",
    "\x1b[m",
    "\x1b[1;4;7m",
    "\x1b[22;23;24;27;39;49m",
    "\x1b[1;38;5;196;48;2;1;2;3m",
    "\x1b[38:2::1:2:3m",
    "\x1b[38:5:9m",
    "\x1b[4:3m",
    "\x1b[;1m",
    "\x1b[1;m",
    "\x1b[38;5;256m",
    "\x1b[99999999999999999999999m",
    "\x1b[5;10H",
    "\x1b[H",
    "\x1b[;7H",
    "\x1b[1;2;3H",
    "\x1b[2J",
    "\x1b[K",
    "\x1b[3A",
    "\x1b[4294967296B",
    "\x1b[2;20r",
    "\x1b[r",
    "\x1b[?25l",
    "\x1b[?1;2004h",
    "\x1b[?2026h",
    "\x1b[?h",
    "\x1b[?6r",
    "\x1b[4h",
    "\x1b[2 q",
    "\x1b[>c",
    "\x1b[6n",
    "\x1b[?1?2h",
    "\x1b[:1m",
    "\x1b]0;title\x07",
    "\x1b7",
    "\x1b(0",
    "\x1bP$qm\x1b\\",
];

const TEXT: &[&str] = &[
    "a",
    "xyz",
    "\u{e9}",
    "\u{4e2d}",
    "\u{1f600}",
    "\u{1f680}",
    "\u{301}",
    "\u{200d}",
];

fn arb_piece() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        6 => proptest::sample::select(SEQUENCES).prop_map(|s| s.as_bytes().to_vec()),
        3 => proptest::collection::vec(
            proptest::sample::select(b"\x1b[0123456789;:?<>= $mHJKABCDrhl".to_vec()),
            1..12,
        ),
        5 => proptest::sample::select(TEXT).prop_map(|s| s.as_bytes().to_vec()),
        2 => proptest::sample::select(vec![
            vec![b'\r', b'\n'],
            vec![0x07],
            vec![0x7f],
            vec![0x18],
            vec![0x9b],
            vec![0xc2, 0x85],
            vec![0xff],
            vec![0xe2, 0x82],
            vec![0xf0, 0x9f, 0x98],
        ]),
    ]
}

proptest! {
    #[test]
    fn csi_fast_path_keeps_the_action_stream(
        pieces in proptest::collection::vec(arb_piece(), 0..48),
        split in any::<proptest::sample::Index>(),
    ) {
        let bytes: Vec<u8> = pieces.concat();
        assert_same_stream(&[&bytes]);
        let at = split.index(bytes.len() + 1);
        assert_same_stream(&[&bytes[..at], &bytes[at..]]);
    }
}

/// Every split point of the operator's T0 shape, including inside the
/// escapes and the 4-byte character, and inside a cached repeat.
#[test]
fn t0_shape_keeps_the_action_stream_at_every_split() {
    let bytes = "\x1b[38;5;196m\x1b[48;5;21m\u{1f600}\x1b[38;5;196m\x1b[48;5;21m\u{1f680}\
                 \x1b[38;5;7m\x1b[48;5;0mx"
        .as_bytes();
    assert_same_stream(&[bytes]);
    for at in 0..=bytes.len() {
        assert_same_stream(&[&bytes[..at], &bytes[at..]]);
    }
    let bytewise: Vec<&[u8]> = bytes.chunks(1).collect();
    assert_same_stream(&bytewise);
}
