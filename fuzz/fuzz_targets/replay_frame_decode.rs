//! Structure-aware fuzz harness for semantic replay frame decoding.
//!
//! `replay_recording_from_bytes` covers the byte-stream frame parser, while this
//! target constructs individual `RecordingFrame` values directly and checks the
//! `decode_frame` payload contract for each frame type.

#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use frankenterm_core::recording::{FrameHeader, FrameType, RecordingFrame};
use frankenterm_core::replay::{DecodedFrame, decode_frame};
use libfuzzer_sys::fuzz_target;

const MAX_RAW_BYTES: usize = 128 * 1024;
const MAX_PAYLOAD_BYTES: usize = 64 * 1024;

#[derive(Arbitrary, Debug)]
struct FrameCase<'a> {
    timestamp_ms: u64,
    frame_type_seed: u8,
    flags: u8,
    declared_len: DeclaredLen,
    payload: &'a [u8],
}

#[derive(Arbitrary, Debug)]
enum DeclaredLen {
    Actual,
    Zero,
    One,
    Huge,
    Fixed(u32),
    Plus(u16),
    Minus(u16),
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_RAW_BYTES {
        return;
    }

    if let Some(frame) = text_seed_frame(data) {
        exercise_frame(&frame);
    }

    let mut unstructured = Unstructured::new(data);
    if let Ok(case) = FrameCase::arbitrary(&mut unstructured) {
        let frame = case.to_frame();
        exercise_frame(&frame);
    }
});

impl FrameCase<'_> {
    fn to_frame(&self) -> RecordingFrame {
        let frame_type = frame_type_from_seed(self.frame_type_seed);
        let payload = cap_slice(self.payload, MAX_PAYLOAD_BYTES).to_vec();
        RecordingFrame {
            header: FrameHeader {
                timestamp_ms: self.timestamp_ms,
                frame_type,
                flags: self.flags,
                payload_len: self.declared_len.for_actual_len(payload.len()),
            },
            payload,
        }
    }
}

impl DeclaredLen {
    fn for_actual_len(&self, actual_len: usize) -> u32 {
        let actual = actual_len.min(u32::MAX as usize);
        match self {
            Self::Actual => actual as u32,
            Self::Zero => 0,
            Self::One => 1,
            Self::Huge => u32::MAX,
            Self::Fixed(len) => *len,
            Self::Plus(delta) => actual
                .saturating_add(usize::from(*delta))
                .min(u32::MAX as usize) as u32,
            Self::Minus(delta) => actual.saturating_sub(usize::from(*delta)) as u32,
        }
    }
}

fn exercise_frame(frame: &RecordingFrame) {
    if frame.header.payload_len as usize != frame.payload.len() {
        assert!(
            decode_frame(frame).is_err(),
            "payload_len mismatch must fail before semantic decoding"
        );
        return;
    }

    match frame.header.frame_type {
        FrameType::Output => {
            let decoded = decode_frame(frame).expect("output frame should decode");
            let DecodedFrame::Output(bytes) = decoded else {
                panic!("output frame decoded to wrong variant");
            };
            assert_eq!(bytes, frame.payload);
        }
        FrameType::Resize => match decode_frame(frame) {
            Ok(DecodedFrame::Resize { cols, rows }) => {
                assert_eq!(frame.payload.len(), 4);
                assert_eq!(
                    cols,
                    u16::from_le_bytes([frame.payload[0], frame.payload[1]])
                );
                assert_eq!(
                    rows,
                    u16::from_le_bytes([frame.payload[2], frame.payload[3]])
                );
            }
            Ok(_) => panic!("resize frame decoded to wrong variant"),
            Err(_) => assert_ne!(frame.payload.len(), 4),
        },
        FrameType::Event => {
            let expected = serde_json::from_slice::<serde_json::Value>(&frame.payload);
            match (expected, decode_frame(frame)) {
                (Ok(expected), Ok(DecodedFrame::Event(actual))) => assert_eq!(actual, expected),
                (Err(_), Err(_)) => {}
                (Ok(_), Ok(_)) | (Err(_), Ok(_)) => {
                    panic!("event frame decoded to unexpected non-event value");
                }
                (Ok(_), Err(err)) => panic!("valid event JSON failed to decode: {err}"),
            }
        }
        FrameType::Marker => {
            let decoded = decode_frame(frame).expect("marker frame should decode");
            let DecodedFrame::Marker(text) = decoded else {
                panic!("marker frame decoded to wrong variant");
            };
            assert_eq!(text, String::from_utf8_lossy(&frame.payload).as_ref());
        }
        FrameType::Input => {
            let decoded = decode_frame(frame).expect("input frame should decode");
            let DecodedFrame::Input(bytes) = decoded else {
                panic!("input frame decoded to wrong variant");
            };
            assert_eq!(bytes, frame.payload);
        }
    }
}

fn text_seed_frame(data: &[u8]) -> Option<RecordingFrame> {
    let text = std::str::from_utf8(data).ok()?;
    let mut lines = text.lines();
    if lines.next()? != "FTFRAME1" {
        return None;
    }

    let spec = lines.next()?;
    let mut parts = spec.splitn(5, ' ');
    let kind = parts.next()?;
    let timestamp_ms = parts.next()?.parse::<u64>().ok()?;
    let flags = parts.next()?.parse::<u8>().ok()?;
    let declared_len = parts.next().and_then(parse_declared_len)?;
    let rest = parts.next().unwrap_or_default();
    let frame_type = text_seed_frame_type(kind)?;
    let payload = text_seed_payload(kind, rest)?;

    Some(RecordingFrame {
        header: FrameHeader {
            timestamp_ms,
            frame_type,
            flags,
            payload_len: declared_len.for_actual_len(payload.len()),
        },
        payload,
    })
}

fn text_seed_frame_type(kind: &str) -> Option<FrameType> {
    match kind {
        "output" => Some(FrameType::Output),
        "resize" | "resize_raw" => Some(FrameType::Resize),
        "event" => Some(FrameType::Event),
        "marker" => Some(FrameType::Marker),
        "input" => Some(FrameType::Input),
        _ => None,
    }
}

fn text_seed_payload(kind: &str, rest: &str) -> Option<Vec<u8>> {
    if kind == "resize" {
        let mut dims = rest.split_whitespace();
        let cols = dims.next()?.parse::<u16>().ok()?;
        let rows = dims.next()?.parse::<u16>().ok()?;
        let mut payload = Vec::with_capacity(4);
        payload.extend_from_slice(&cols.to_le_bytes());
        payload.extend_from_slice(&rows.to_le_bytes());
        Some(payload)
    } else {
        Some(rest.as_bytes().to_vec())
    }
}

fn parse_declared_len(token: &str) -> Option<DeclaredLen> {
    match token {
        "actual" => Some(DeclaredLen::Actual),
        "zero" => Some(DeclaredLen::Zero),
        "one" => Some(DeclaredLen::One),
        "huge" => Some(DeclaredLen::Huge),
        value => value.parse::<u32>().ok().map(DeclaredLen::Fixed),
    }
}

fn frame_type_from_seed(seed: u8) -> FrameType {
    match seed % 5 {
        0 => FrameType::Output,
        1 => FrameType::Resize,
        2 => FrameType::Event,
        3 => FrameType::Marker,
        _ => FrameType::Input,
    }
}

fn cap_slice(bytes: &[u8], max_len: usize) -> &[u8] {
    &bytes[..bytes.len().min(max_len)]
}
