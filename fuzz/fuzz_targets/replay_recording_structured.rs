//! Structure-aware fuzz harness for WAR replay recording decoding.
//!
//! Complements `replay_recording_from_bytes`, which feeds arbitrary raw bytes
//! into `Recording::from_bytes`. This target builds syntactically valid frame
//! sequences, mutates the encoded stream at frame boundaries, and asserts that
//! exact structured recordings parse, re-encode, and semantically decode.

#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use frankenterm_core::recording::{FrameHeader, FrameType, RecordingFrame};
use frankenterm_core::replay::{Recording, decode_frame};
use libfuzzer_sys::fuzz_target;

const FRAME_HEADER_LEN: usize = 14;
const MAX_RAW_BYTES: usize = 8 * 1024 * 1024;
const MAX_FRAMES: usize = 16;
const MAX_PAYLOAD_BYTES: usize = 4 * 1024;
const MAX_APPEND_BYTES: usize = 1_024;

#[derive(Arbitrary, Debug)]
struct StructuredRecording<'a> {
    frames: Vec<StructuredFrame<'a>>,
    mutation: RecordingMutation<'a>,
}

#[derive(Arbitrary, Debug)]
struct StructuredFrame<'a> {
    timestamp_ms: u64,
    kind_seed: u8,
    flags: u8,
    payload: &'a [u8],
    cols: u16,
    rows: u16,
    event_seed: u8,
}

#[derive(Arbitrary, Debug)]
enum RecordingMutation<'a> {
    Exact,
    Truncate(u16),
    Append(&'a [u8]),
    FlipByte { offset_seed: u16, xor: u8 },
    OverridePayloadLen { frame_index_seed: u8, len: u32 },
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_RAW_BYTES {
        return;
    }

    let _ = Recording::from_bytes(data);
    if let Some(frames) = text_seed_frames(data) {
        exercise_frames(frames, &RecordingMutation::Exact);
    }

    let mut unstructured = Unstructured::new(data);
    if let Ok(input) = StructuredRecording::arbitrary(&mut unstructured) {
        let frames = input
            .frames
            .into_iter()
            .take(MAX_FRAMES)
            .map(|frame| frame.to_recording_frame())
            .collect::<Vec<_>>();
        exercise_frames(frames, &input.mutation);
    }
});

impl StructuredFrame<'_> {
    fn to_recording_frame(&self) -> RecordingFrame {
        let frame_type = match self.kind_seed % 5 {
            0 => FrameType::Output,
            1 => FrameType::Resize,
            2 => FrameType::Event,
            3 => FrameType::Marker,
            _ => FrameType::Input,
        };
        let payload = self.payload_for(frame_type);
        RecordingFrame {
            header: FrameHeader {
                timestamp_ms: self.timestamp_ms,
                frame_type,
                flags: self.flags,
                payload_len: payload.len() as u32,
            },
            payload,
        }
    }

    fn payload_for(&self, frame_type: FrameType) -> Vec<u8> {
        match frame_type {
            FrameType::Output | FrameType::Input | FrameType::Marker => {
                cap_slice(self.payload, MAX_PAYLOAD_BYTES).to_vec()
            }
            FrameType::Resize => {
                let mut payload = Vec::with_capacity(4);
                payload.extend_from_slice(&self.cols.to_le_bytes());
                payload.extend_from_slice(&self.rows.to_le_bytes());
                payload
            }
            FrameType::Event => {
                let value = match self.event_seed % 4 {
                    0 => serde_json::json!({}),
                    1 => serde_json::json!({
                        "kind": "fuzz_event",
                        "payload_len": self.payload.len().min(MAX_PAYLOAD_BYTES),
                    }),
                    2 => serde_json::json!({
                        "flags": self.flags,
                        "timestamp_ms": self.timestamp_ms,
                    }),
                    _ => serde_json::json!([
                        "frame",
                        self.kind_seed,
                        self.payload.len().min(MAX_PAYLOAD_BYTES),
                    ]),
                };
                serde_json::to_vec(&value).expect("fuzz event JSON serializes")
            }
        }
    }
}

fn exercise_frames(frames: Vec<RecordingFrame>, mutation: &RecordingMutation<'_>) {
    let exact = matches!(mutation, RecordingMutation::Exact);
    let mut bytes = encode_frames(&frames);
    mutation.apply(&frames, &mut bytes);

    match Recording::from_bytes(&bytes) {
        Ok(recording) => {
            if exact {
                assert_eq!(recording.frames.len(), frames.len());
                assert_eq!(encode_frames(&recording.frames), bytes);
                assert_eq!(
                    recording.duration_ms,
                    frames.last().map_or(0, |frame| frame.header.timestamp_ms)
                );
            }

            for frame in &recording.frames {
                if exact {
                    decode_frame(frame).expect("exact structured frame decodes");
                } else {
                    let _ = decode_frame(frame);
                }
            }
        }
        Err(_) => {
            assert!(
                !exact,
                "exact structured recording should parse without decoder errors"
            );
        }
    }
}

impl RecordingMutation<'_> {
    fn apply(&self, frames: &[RecordingFrame], bytes: &mut Vec<u8>) {
        match self {
            Self::Exact => {}
            Self::Truncate(seed) => {
                let new_len = if bytes.is_empty() {
                    0
                } else {
                    usize::from(*seed) % (bytes.len() + 1)
                };
                bytes.truncate(new_len);
            }
            Self::Append(extra) => {
                bytes.extend_from_slice(cap_slice(extra, MAX_APPEND_BYTES));
            }
            Self::FlipByte { offset_seed, xor } => {
                if !bytes.is_empty() {
                    let offset = usize::from(*offset_seed) % bytes.len();
                    bytes[offset] ^= *xor;
                }
            }
            Self::OverridePayloadLen {
                frame_index_seed,
                len,
            } => {
                if let Some(offset) = frame_header_offset(frames, *frame_index_seed) {
                    let payload_len_offset = offset + 10;
                    if bytes.len() >= payload_len_offset + 4 {
                        bytes[payload_len_offset..payload_len_offset + 4]
                            .copy_from_slice(&len.to_le_bytes());
                    }
                }
            }
        }
    }
}

fn encode_frames(frames: &[RecordingFrame]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for frame in frames {
        bytes.extend(frame.encode());
    }
    bytes
}

fn frame_header_offset(frames: &[RecordingFrame], seed: u8) -> Option<usize> {
    if frames.is_empty() {
        return None;
    }
    let target = usize::from(seed) % frames.len();
    let mut offset = 0usize;
    for frame in &frames[..target] {
        offset = offset
            .saturating_add(FRAME_HEADER_LEN)
            .saturating_add(frame.payload.len());
    }
    Some(offset)
}

fn cap_slice(bytes: &[u8], max_len: usize) -> &[u8] {
    &bytes[..bytes.len().min(max_len)]
}

fn text_seed_frames(data: &[u8]) -> Option<Vec<RecordingFrame>> {
    let text = std::str::from_utf8(data).ok()?;
    let mut lines = text.lines();
    if lines.next()? != "FTWAR1" {
        return None;
    }

    let mut frames = Vec::new();
    for line in lines.take(MAX_FRAMES) {
        if let Some(frame) = parse_text_seed_line(line) {
            frames.push(frame);
        }
    }
    Some(frames)
}

fn parse_text_seed_line(line: &str) -> Option<RecordingFrame> {
    let mut parts = line.splitn(3, ' ');
    let kind = parts.next()?;
    let timestamp_ms = parts.next()?.parse::<u64>().ok()?;
    let rest = parts.next().unwrap_or_default();

    let (frame_type, payload) = match kind {
        "output" => (FrameType::Output, rest.as_bytes().to_vec()),
        "input" => (FrameType::Input, rest.as_bytes().to_vec()),
        "marker" => (FrameType::Marker, rest.as_bytes().to_vec()),
        "event" => {
            let value: serde_json::Value = serde_json::from_str(rest).ok()?;
            (FrameType::Event, serde_json::to_vec(&value).ok()?)
        }
        "resize" => {
            let mut dims = rest.split_whitespace();
            let cols = dims.next()?.parse::<u16>().ok()?;
            let rows = dims.next()?.parse::<u16>().ok()?;
            let mut payload = Vec::with_capacity(4);
            payload.extend_from_slice(&cols.to_le_bytes());
            payload.extend_from_slice(&rows.to_le_bytes());
            (FrameType::Resize, payload)
        }
        _ => return None,
    };

    Some(RecordingFrame {
        header: FrameHeader {
            timestamp_ms,
            frame_type,
            flags: 0,
            payload_len: payload.len() as u32,
        },
        payload,
    })
}
