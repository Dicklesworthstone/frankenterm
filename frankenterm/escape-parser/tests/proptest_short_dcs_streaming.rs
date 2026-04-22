//! Property-based streaming tests for short DCS parsing.
//!
//! The core parser has unit coverage for bulk short-DCS parsing and for the
//! over-limit truncation path, but it did not separately prove the same
//! behavior under chunked delivery. These tests harden the parser's streaming
//! contract:
//!
//! 1. Complete DECRQSS short-DCS sequences parse identically in bulk and when
//!    fed in arbitrary chunks.
//! 2. Over-limit short-DCS payloads still recover cleanly after the ST
//!    terminator, and the following sequence/text is parsed the same way under
//!    streaming delivery.

use frankenterm_escape_parser::parser::Parser;
use frankenterm_escape_parser::{Action, DeviceControlMode, ShortDeviceControl};
use proptest::prelude::*;

/// Keep this in sync with parser/mod.rs MAX_SHORT_DCS_BYTES.
const SHORT_DCS_LIMIT_BYTES: usize = 8 * 1024 * 1024;

fn arb_params() -> impl Strategy<Value = Vec<u16>> {
    prop::collection::vec(0u16..1000u16, 0..4)
}

fn arb_short_dcs_data() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(0x20u8..0x7fu8, 0..128)
}

fn arb_chunk_sizes() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(1u8..32u8, 1..24)
}

fn build_short_dcs(params: &[u16], data: &[u8]) -> Vec<u8> {
    let mut seq = vec![0x1b, b'P'];
    for (idx, param) in params.iter().enumerate() {
        if idx > 0 {
            seq.push(b';');
        }
        seq.extend_from_slice(param.to_string().as_bytes());
    }
    seq.extend_from_slice(b"$q");
    seq.extend_from_slice(data);
    seq.extend_from_slice(b"\x1b\\");
    seq
}

fn parse_bulk(bytes: &[u8]) -> Vec<Action> {
    let mut parser = Parser::new();
    parser.parse_as_vec(bytes)
}

fn parse_segments<'a>(segments: impl IntoIterator<Item = &'a [u8]>) -> Vec<Action> {
    let mut parser = Parser::new();
    let mut out = Vec::new();
    for segment in segments {
        for action in parser.parse_as_vec(segment) {
            action.append_to(&mut out);
        }
    }
    out
}

fn parse_chunked(bytes: &[u8], chunk_sizes: &[u8]) -> Vec<Action> {
    let mut start = 0usize;
    let mut idx = 0usize;
    let segments = std::iter::from_fn(|| {
        if start >= bytes.len() {
            return None;
        }
        let size = usize::from(chunk_sizes[idx % chunk_sizes.len()]);
        idx += 1;
        let end = (start + size).min(bytes.len());
        let segment = &bytes[start..end];
        start = end;
        Some(segment)
    });
    parse_segments(segments)
}

fn collect_short_dcs(actions: &[Action]) -> Vec<ShortDeviceControl> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::DeviceControl(DeviceControlMode::ShortDeviceControl(dcs)) => {
                Some((**dcs).clone())
            }
            _ => None,
        })
        .collect()
}

fn render(actions: &[Action]) -> String {
    actions.iter().map(ToString::to_string).collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn short_dcs_chunked_matches_bulk(
        params in arb_params(),
        data in arb_short_dcs_data(),
        chunk_sizes in arb_chunk_sizes(),
    ) {
        let seq = build_short_dcs(&params, &data);
        let bulk = parse_bulk(&seq);
        let chunked = parse_chunked(&seq, &chunk_sizes);

        prop_assert_eq!(&chunked, &bulk, "chunked parser output diverged from bulk parse");

        let short_dcs = collect_short_dcs(&chunked);
        prop_assert_eq!(short_dcs.len(), 1, "complete DECRQSS sequence should emit exactly one short DCS");
        prop_assert_eq!(
            &short_dcs[0].params,
            &params
                .iter()
                .map(|&param| i64::from(param))
                .collect::<Vec<_>>()
        );
        prop_assert_eq!(&short_dcs[0].intermediates, &vec![b'$']);
        prop_assert_eq!(short_dcs[0].byte, b'q');
        prop_assert_eq!(&short_dcs[0].data, &data);
    }
}

#[test]
fn overlong_short_dcs_chunked_recovers_after_terminator() {
    let mut seq = Vec::with_capacity(SHORT_DCS_LIMIT_BYTES + 512);
    seq.extend_from_slice(b"\x1bP$q");
    seq.extend(std::iter::repeat_n(b'x', SHORT_DCS_LIMIT_BYTES + 257));
    seq.extend_from_slice(b"\x1b\\");
    seq.extend_from_slice(b"\x1bP$qOK\x1b\\tail");

    let bulk = parse_bulk(&seq);

    let split_a = 2usize;
    let split_b = 5usize;
    let split_c = 5 + SHORT_DCS_LIMIT_BYTES + 17;
    let split_d = 5 + SHORT_DCS_LIMIT_BYTES + 257;
    let chunked = parse_segments([
        &seq[..split_a],
        &seq[split_a..split_b],
        &seq[split_b..split_c],
        &seq[split_c..split_d],
        &seq[split_d..],
    ]);

    assert_eq!(
        collect_short_dcs(&chunked),
        collect_short_dcs(&bulk),
        "chunked over-limit short DCS parsing diverged from bulk short-DCS recovery"
    );
    assert_eq!(
        render(&chunked),
        render(&bulk),
        "chunked over-limit short DCS parsing diverged from bulk rendered output"
    );

    let short_dcs = collect_short_dcs(&chunked);
    assert_eq!(
        short_dcs.len(),
        2,
        "expected truncated over-limit DCS plus post-recovery DCS"
    );
    assert_eq!(short_dcs[0].data.len(), SHORT_DCS_LIMIT_BYTES);
    assert!(short_dcs[0].data.iter().all(|&byte| byte == b'x'));
    assert_eq!(short_dcs[1].data, b"OK".to_vec());
    assert!(
        render(&chunked).ends_with("tail"),
        "parser must recover after the ST terminator and resume printable output"
    );
}
