//! Equivalence gate for validating a printable run's UTF-8 in one pass
//! (ft-yccm0.3.2.3).
//!
//! With `simd_utf8` on (the default) a run's extent is found with the ASCII
//! scans, its UTF-8 validated once, and a lone character decoded in place.
//! Off (`FT_PARSER_SIMD_UTF8=0`), every character is checked on its own: the
//! oracle. Malformed input must come out identically: the state machine
//! decodes it either way, so replacement characters land in the same places,
//! including for sequences split across chunks. The streams mix valid UTF-8
//! of every length with overlongs, surrogates, values past U+10FFFF, stray
//! continuations, truncated sequences, C1 controls encoded in UTF-8,
//! controls and escapes.

use frankenterm_escape_parser::Action;
use frankenterm_escape_parser::parser::{AsciiScan, Parser};
use proptest::prelude::*;

fn parse_chunks(chunks: &[&[u8]], simd_utf8: bool, scan: AsciiScan) -> Vec<Action> {
    let mut parser = Parser::new();
    parser.set_print_batching(true);
    parser.set_simd_utf8(simd_utf8);
    parser.set_ascii_scan(scan);
    assert_eq!(parser.simd_utf8(), simd_utf8);
    let mut actions = Vec::new();
    for chunk in chunks {
        parser.parse(chunk, |action| actions.push(action));
    }
    actions
}

fn assert_same_stream(chunks: &[&[u8]]) {
    let oracle = parse_chunks(chunks, false, AsciiScan::Scalar);
    for scan in AsciiScan::ALL {
        assert_eq!(
            parse_chunks(chunks, true, scan),
            oracle,
            "{:?} on chunks {:?}",
            scan,
            chunks
        );
    }
}

const PIECES: &[&[u8]] = &[
    b"a",
    b"plain ascii text ",
    "caf\u{e9} na\u{ef}ve \u{a0}\u{a9}\u{ae}".as_bytes(),
    "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2510}".as_bytes(),
    "\u{4e2d}\u{6587}\u{5b57}".as_bytes(),
    "\u{1f600}".as_bytes(),
    "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}".as_bytes(),
    "\u{2764}\u{fe0f}".as_bytes(),
    "e\u{301}".as_bytes(),
    "\u{85}".as_bytes(),
    "\u{9b}31m".as_bytes(),
    "\u{10ffff}\u{fffd}\u{ffff}".as_bytes(),
    b"\x1b[38;5;196m",
    b"\x1b[48;5;21m",
    b"\x1b]0;t\x07",
    b"\r\n",
    b"\x07",
    b"\x7f",
    b"\x80",
    b"\xbf\xbf",
    b"\xc0\x80",
    b"\xc1\xbf",
    b"\xe0\x80\x80",
    b"\xe0\x9f\xbf",
    b"\xed\xa0\x80",
    b"\xed\xbf\xbf",
    b"\xf0\x80\x80\x80",
    b"\xf0\x8f\xbf\xbf",
    b"\xf4\x90\x80\x80",
    b"\xf5\x80\x80\x80",
    b"\xf8\x88\x80\x80\x80",
    b"\xe2\x82",
    b"\xf0\x9f\x98",
    b"\xc3",
    b"\xff\xfe",
];

proptest! {
    #[test]
    fn one_pass_utf8_runs_keep_the_action_stream(
        pieces in proptest::collection::vec(proptest::sample::select(PIECES), 0..40),
        split in any::<proptest::sample::Index>(),
    ) {
        let bytes: Vec<u8> = pieces.concat();
        assert_same_stream(&[&bytes]);
        let at = split.index(bytes.len() + 1);
        assert_same_stream(&[&bytes[..at], &bytes[at..]]);
    }
}

/// The operator's T0 cells, a box-drawing line and malformed sequences, at
/// every split point and a byte at a time.
#[test]
fn utf8_runs_keep_the_action_stream_at_every_split() {
    let cases: &[&[u8]] = &[
        "\x1b[38;5;196m\x1b[48;5;21m\u{1f600}\x1b[38;5;7m\x1b[48;5;0m\u{1f680}x".as_bytes(),
        "\u{250c}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2510}\r\n\u{2502} \u{4e2d} \u{2502}"
            .as_bytes(),
        b"a\xc3\xa9\xff\xc3\xa9\x80\xc3\xa9\xc3\xc3\xa9\xe2\x82\xc3\xa9\xed\xa0\x80z",
        b"x\xc2\x85y\xc2\xa0z\xc2\x9b31mw",
        b"\xf0\x9f\x98\x80\xf0\x9f\x98\x80\xf0\x9f\x98",
    ];
    for bytes in cases {
        assert_same_stream(&[bytes]);
        for at in 0..=bytes.len() {
            assert_same_stream(&[&bytes[..at], &bytes[at..]]);
        }
        let bytewise: Vec<&[u8]> = bytes.chunks(1).collect();
        assert_same_stream(&bytewise);
    }
}
