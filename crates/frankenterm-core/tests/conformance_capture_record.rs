//! Conformance harness for the capture-record binary wire format.
//!
//! The decoder contract lives in `replay::parse_frame` / `Recording::from_bytes`:
//!
//! ```text
//! timestamp_ms : u64 little-endian
//! frame_type   : u8  (1=Output, 2=Resize, 3=Event, 4=Marker, 5=Input)
//! flags        : u8
//! payload_len  : u32 little-endian
//! payload      : [u8; payload_len]
//! ```
//!
//! These tests hand-build wire bytes so the decoder is exercised directly,
//! without routing through the encoder first.

use frankenterm_core::recording::{FrameHeader, FrameType, RecordingFrame};
use frankenterm_core::replay::Recording;

const FRAME_HEADER_LEN: usize = 14;

fn frame_type_tag(frame_type: FrameType) -> u8 {
    match frame_type {
        FrameType::Output => 1,
        FrameType::Resize => 2,
        FrameType::Event => 3,
        FrameType::Marker => 4,
        FrameType::Input => 5,
    }
}

fn raw_frame(
    ts: u64,
    frame_type_byte: u8,
    flags: u8,
    claimed_payload_len: u32,
    payload: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    buf.extend_from_slice(&ts.to_le_bytes());
    buf.push(frame_type_byte);
    buf.push(flags);
    buf.extend_from_slice(&claimed_payload_len.to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn parse_single_frame(bytes: &[u8]) -> frankenterm_core::recording::RecordingFrame {
    let recording = Recording::from_bytes(bytes).expect("frame should parse");
    assert_eq!(
        recording.frames.len(),
        1,
        "expected exactly one parsed frame"
    );
    recording.frames.into_iter().next().unwrap()
}

#[test]
fn conformance_constants_still_match_encoder() {
    let tags = [
        (FrameType::Output, 1u8),
        (FrameType::Resize, 2),
        (FrameType::Event, 3),
        (FrameType::Marker, 4),
        (FrameType::Input, 5),
    ];

    for (frame_type, expected_tag) in tags {
        let encoded = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: 0,
                frame_type,
                flags: 0,
                payload_len: 0,
            },
            payload: Vec::new(),
        }
        .encode();

        assert_eq!(encoded.len(), FRAME_HEADER_LEN);
        assert_eq!(
            encoded[8], expected_tag,
            "encoder tag drift for {frame_type:?}"
        );
    }
}

#[test]
fn conformance_empty_stream_is_valid_empty_recording() {
    let recording = Recording::from_bytes(&[]).expect("empty stream should be accepted");
    assert!(recording.frames.is_empty());
    assert_eq!(recording.duration_ms, 0);
}

#[test]
fn conformance_minimal_zero_payload_round_trip() {
    let bytes = raw_frame(0, frame_type_tag(FrameType::Output), 0, 0, &[]);
    let frame = parse_single_frame(&bytes);

    assert_eq!(frame.header.timestamp_ms, 0);
    assert_eq!(frame.header.frame_type, FrameType::Output);
    assert_eq!(frame.header.flags, 0);
    assert_eq!(frame.header.payload_len, 0);
    assert!(frame.payload.is_empty());
}

#[test]
fn conformance_max_field_values_round_trip() {
    let bytes = raw_frame(
        u64::MAX,
        frame_type_tag(FrameType::Input),
        u8::MAX,
        1,
        &[0x7f],
    );
    let frame = parse_single_frame(&bytes);

    assert_eq!(frame.header.timestamp_ms, u64::MAX);
    assert_eq!(frame.header.frame_type, FrameType::Input);
    assert_eq!(frame.header.flags, u8::MAX);
    assert_eq!(frame.header.payload_len, 1);
    assert_eq!(frame.payload, vec![0x7f]);
}

#[test]
fn conformance_boundary_payload_len_one_round_trip() {
    let data = payload(1);
    let bytes = raw_frame(
        11,
        frame_type_tag(FrameType::Output),
        0,
        data.len() as u32,
        &data,
    );
    let frame = parse_single_frame(&bytes);
    assert_eq!(frame.header.payload_len as usize, data.len());
    assert_eq!(frame.payload, data);
}

#[test]
fn conformance_boundary_payload_len_255_round_trip() {
    let data = payload(255);
    let bytes = raw_frame(
        22,
        frame_type_tag(FrameType::Output),
        0,
        data.len() as u32,
        &data,
    );
    let frame = parse_single_frame(&bytes);
    assert_eq!(frame.header.payload_len as usize, data.len());
    assert_eq!(frame.payload, data);
}

#[test]
fn conformance_boundary_payload_len_256_round_trip() {
    let data = payload(256);
    let bytes = raw_frame(
        33,
        frame_type_tag(FrameType::Output),
        0,
        data.len() as u32,
        &data,
    );
    let frame = parse_single_frame(&bytes);
    assert_eq!(frame.header.payload_len as usize, data.len());
    assert_eq!(frame.payload, data);
}

#[test]
fn conformance_boundary_payload_len_65535_round_trip() {
    let data = payload(65_535);
    let bytes = raw_frame(
        44,
        frame_type_tag(FrameType::Marker),
        0,
        data.len() as u32,
        &data,
    );
    let frame = parse_single_frame(&bytes);
    assert_eq!(frame.header.payload_len as usize, data.len());
    assert_eq!(frame.payload.len(), data.len());
    assert_eq!(frame.payload, data);
}

#[test]
fn conformance_boundary_payload_len_65536_round_trip() {
    let data = payload(65_536);
    let bytes = raw_frame(
        55,
        frame_type_tag(FrameType::Event),
        0,
        data.len() as u32,
        &data,
    );
    let frame = parse_single_frame(&bytes);
    assert_eq!(frame.header.payload_len as usize, data.len());
    assert_eq!(frame.payload, data);
}

#[test]
fn conformance_two_frames_round_trip_preserves_order() {
    let first = raw_frame(10, frame_type_tag(FrameType::Output), 0, 2, b"hi");
    let second = raw_frame(25, frame_type_tag(FrameType::Marker), 1, 4, b"done");

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&first);
    bytes.extend_from_slice(&second);

    let recording = Recording::from_bytes(&bytes).expect("concatenated frames should parse");
    assert_eq!(recording.frames.len(), 2);
    assert_eq!(recording.frames[0].header.timestamp_ms, 10);
    assert_eq!(recording.frames[0].header.frame_type, FrameType::Output);
    assert_eq!(recording.frames[0].payload, b"hi");
    assert_eq!(recording.frames[1].header.timestamp_ms, 25);
    assert_eq!(recording.frames[1].header.frame_type, FrameType::Marker);
    assert_eq!(recording.frames[1].header.flags, 1);
    assert_eq!(recording.frames[1].payload, b"done");
}

#[test]
fn conformance_non_canonical_unknown_flag_bits_are_preserved() {
    let bytes = raw_frame(
        99,
        frame_type_tag(FrameType::Output),
        0b1010_0101,
        3,
        b"raw",
    );
    let frame = parse_single_frame(&bytes);

    assert_eq!(frame.header.frame_type, FrameType::Output);
    assert_eq!(frame.header.flags, 0b1010_0101);
    assert_eq!(frame.payload, b"raw");
}

#[test]
fn conformance_invalid_frame_type_byte_is_rejected() {
    let err =
        Recording::from_bytes(&raw_frame(0, 0, 0, 0, &[])).expect_err("invalid type must fail");
    let message = err.to_string();
    assert!(
        message.contains("unknown frame type byte"),
        "unexpected error: {message}"
    );
}

#[test]
fn conformance_truncated_header_is_rejected() {
    let err = Recording::from_bytes(&[0u8; 13]).expect_err("short header must fail");
    let message = err.to_string();
    assert!(
        message.contains("unexpected EOF reading frame header"),
        "unexpected error: {message}"
    );
}

#[test]
fn conformance_truncated_payload_is_rejected() {
    let err = Recording::from_bytes(&raw_frame(
        0,
        frame_type_tag(FrameType::Output),
        0,
        8,
        b"abc",
    ))
    .expect_err("short payload must fail");
    let message = err.to_string();
    assert!(
        message.contains("unexpected EOF reading frame payload"),
        "unexpected error: {message}"
    );
}

#[test]
fn conformance_max_claimed_payload_len_without_body_is_rejected() {
    let err = Recording::from_bytes(&raw_frame(
        0,
        frame_type_tag(FrameType::Output),
        0,
        u32::MAX,
        &[],
    ))
    .expect_err("u32::MAX payload claim without bytes must fail");
    let message = err.to_string();
    assert!(
        message.contains("unexpected EOF reading frame payload"),
        "unexpected error: {message}"
    );
}

#[test]
fn conformance_trailing_garbage_after_valid_frame_is_rejected() {
    let mut bytes = raw_frame(7, frame_type_tag(FrameType::Output), 0, 2, b"ok");
    bytes.extend_from_slice(b"xyz");

    let err = Recording::from_bytes(&bytes).expect_err("trailing garbage must fail");
    let message = err.to_string();
    assert!(
        message.contains("unexpected EOF reading frame header"),
        "unexpected error: {message}"
    );
}

#[test]
fn conformance_truncated_second_frame_is_rejected() {
    let mut bytes = raw_frame(7, frame_type_tag(FrameType::Output), 0, 2, b"ok");
    bytes.extend_from_slice(&[0u8; 10]);

    let err = Recording::from_bytes(&bytes).expect_err("partial second frame must fail");
    let message = err.to_string();
    assert!(
        message.contains("unexpected EOF reading frame header"),
        "unexpected error: {message}"
    );
}
