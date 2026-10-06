//! Equivalence gate for the ground-state ASCII scan (ft-yccm0.3.2.2).
//!
//! Every `AsciiScan` (the scalar oracle that `FT_PARSER_SIMD=0` selects, and
//! the 16, 32 and 64 byte `std::simd` scans) must give the parser the same
//! runs, so the action stream is identical for the same chunking. The
//! streams mix long ASCII lines (longer than every SIMD block), UTF-8,
//! controls, escapes and malformed bytes. Pure-ASCII runs must reach
//! `Handler::print_ascii_run`, and everything else the handler methods they
//! reached before.

use frankenterm_escape_parser::csi::{Intensity, Sgr};
use frankenterm_escape_parser::parser::{AsciiScan, Handler, Parser};
use frankenterm_escape_parser::{Action, CSI, ControlCode};
use proptest::prelude::*;

fn parse_chunks(chunks: &[&[u8]], scan: AsciiScan) -> Vec<Action> {
    let mut parser = Parser::new();
    parser.set_print_batching(true);
    parser.set_ascii_scan(scan);
    assert_eq!(parser.ascii_scan(), scan);
    let mut actions = Vec::new();
    for chunk in chunks {
        parser.parse(chunk, |action| actions.push(action));
    }
    actions
}

fn assert_every_scan_agrees(chunks: &[&[u8]]) {
    let oracle = parse_chunks(chunks, AsciiScan::Scalar);
    for scan in AsciiScan::ALL {
        assert_eq!(
            parse_chunks(chunks, scan),
            oracle,
            "{:?} on chunks {:?}",
            scan,
            chunks
        );
    }
}

const PIECES: &[&str] = &[
    "a",
    "hello world",
    "0123456789abcdef0123456789ABCDEF",
    "the quick brown fox jumps over the lazy dog, again and again and again!",
    " ",
    "~",
    "\u{e9}",
    "\u{4e2d}\u{6587}",
    "\u{1f600}",
    "e\u{301}",
    "\r\n",
    "\t",
    "\x07",
    "\x7f",
    "\x1b[38;5;196m",
    "\x1b[0m",
    "\x1b[5;10H",
    "\x1b]0;title\x07",
    "\x1b(0lqk\x1b(B",
];

fn arb_piece() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        8 => proptest::sample::select(PIECES).prop_map(|s| s.as_bytes().to_vec()),
        3 => (1usize..200).prop_map(|len| {
            (0..len).map(|i| b' ' + (i % 95) as u8).collect::<Vec<u8>>()
        }),
        1 => proptest::sample::select(vec![
            vec![0xff],
            vec![0xc2, 0x85],
            vec![0xe2, 0x82],
            vec![0x9b],
            vec![0xc0, 0xaf],
        ]),
    ]
}

proptest! {
    #[test]
    fn every_ascii_scan_gives_the_same_action_stream(
        pieces in proptest::collection::vec(arb_piece(), 0..32),
        split in any::<proptest::sample::Index>(),
    ) {
        let bytes: Vec<u8> = pieces.concat();
        assert_every_scan_agrees(&[&bytes]);
        let at = split.index(bytes.len() + 1);
        assert_every_scan_agrees(&[&bytes[..at], &bytes[at..]]);
    }
}

/// A line longer than every SIMD block, split at every point (so ASCII runs
/// cross chunk boundaries) and fed a byte at a time.
#[test]
fn long_ascii_line_at_every_split_and_bytewise() {
    let line = "x".repeat(150) + "\r\n" + &"y".repeat(65) + "\x1b[1m" + &"z".repeat(17);
    let bytes = line.as_bytes();
    assert_every_scan_agrees(&[bytes]);
    for at in 0..=bytes.len() {
        assert_every_scan_agrees(&[&bytes[..at], &bytes[at..]]);
    }
    let bytewise: Vec<&[u8]> = bytes.chunks(1).collect();
    assert_every_scan_agrees(&bytewise);
}

/// Which handler method each kind of printable run reaches.
#[test]
fn pure_ascii_runs_reach_print_ascii_run() {
    #[derive(Debug, PartialEq)]
    enum Call {
        Ascii(String),
        Str(String),
        Char(char),
        Other(Action),
    }
    #[derive(Default)]
    struct Calls(Vec<Call>);
    impl Handler for Calls {
        fn action(&mut self, action: Action) {
            self.0.push(Call::Other(action));
        }
        fn print(&mut self, c: char) {
            self.0.push(Call::Char(c));
        }
        fn print_str(&mut self, text: &str) {
            self.0.push(Call::Str(text.to_string()));
        }
        fn print_ascii_run(&mut self, run: &str) {
            self.0.push(Call::Ascii(run.to_string()));
        }
    }

    let long = "w".repeat(100);
    let bytes = format!("hello world\r\nh\u{e9}llo\x1b[1ma\x1b[m{long}");
    for scan in AsciiScan::ALL {
        let mut parser = Parser::new();
        parser.set_print_batching(true);
        parser.set_ascii_scan(scan);
        let mut calls = Calls::default();
        parser.parse_with(bytes.as_bytes(), &mut calls);
        let sgr = |sgr: Sgr| Call::Other(Action::CSI(CSI::Sgr(sgr)));
        assert_eq!(
            calls.0,
            vec![
                Call::Ascii("hello world".to_string()),
                Call::Other(Action::Control(ControlCode::CarriageReturn)),
                Call::Other(Action::Control(ControlCode::LineFeed)),
                Call::Str("h\u{e9}llo".to_string()),
                sgr(Sgr::Intensity(Intensity::Bold)),
                Call::Char('a'),
                sgr(Sgr::Reset),
                Call::Ascii(long.clone()),
            ],
            "{:?}",
            scan
        );
    }
}
