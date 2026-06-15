//! Golden + property tests for the WAR recording frame codec safety.
//!
//! The on-disk `.war` format is a sequence of length-prefixed frames:
//!   `[u64 LE timestamp_ms][u8 frame_type][u8 flags][u32 LE payload_len][payload]`
//! `RecordingFrame::encode` writes it; `Recording::from_bytes` reads it.
//!
//! These pin two safety invariants (regression guards for the recorder/replay
//! audit):
//!   1. ROUND-TRIP, no truncation/data-loss: every encoded frame decodes back
//!      byte-for-byte, no frame dropped.
//!   2. CORRUPT INPUT is rejected, no OOM/panic: `from_bytes` on arbitrary bytes
//!      never panics; a frame whose declared `payload_len` exceeds the bytes
//!      actually present is rejected by a bounds-check rather than allocating
//!      (the decoder slices into the input, so it can never allocate more than
//!      the input length — a corrupt ~4 GiB length prefix cannot OOM).

use proptest::prelude::*;

use frankenterm_core::recording::{FrameType, RecordingFrame};
use frankenterm_core::replay::Recording;

// ---------------------------------------------------------------------------
// Golden
// ---------------------------------------------------------------------------

/// Freeze the exact on-disk WAR frame byte layout. A drift here silently
/// corrupts every existing `.war` recording.
#[test]
fn war_frame_encode_byte_layout_golden() {
    let frame = RecordingFrame::new(0x0102_0304_0506_0708, FrameType::Event, 0xAB, b"xy".to_vec())
        .expect("small payload builds a valid frame");
    let bytes = frame.encode();
    let expected: Vec<u8> = vec![
        0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // timestamp_ms (u64 LE)
        0x03, // FrameType::Event = 3
        0xAB, // flags
        0x02, 0x00, 0x00, 0x00, // payload_len = 2 (u32 LE)
        b'x', b'y', // payload
    ];
    assert_eq!(bytes, expected, "WAR frame on-disk byte layout drifted");
}

/// A fixed set of frames (including an empty payload and a >255-byte payload)
/// encodes and decodes back with no frame dropped and no payload truncation.
#[test]
fn war_frame_roundtrip_no_data_loss_golden() {
    let frames = vec![
        RecordingFrame::new(1000, FrameType::Output, 0, b"hello".to_vec()).unwrap(),
        RecordingFrame::new(1001, FrameType::Event, 1, Vec::new()).unwrap(),
        RecordingFrame::new(1002, FrameType::Input, 0, vec![0xFF_u8; 300]).unwrap(),
    ];
    let mut bytes = Vec::new();
    for frame in &frames {
        bytes.extend_from_slice(&frame.encode());
    }

    let recording = Recording::from_bytes(&bytes).expect("a valid WAR stream must decode");
    assert_eq!(
        recording.frames.len(),
        frames.len(),
        "no frame may be dropped on replay"
    );
    for (decoded, original) in recording.frames.iter().zip(frames.iter()) {
        assert_eq!(decoded.header.timestamp_ms, original.header.timestamp_ms);
        assert_eq!(decoded.header.payload_len, original.header.payload_len);
        assert_eq!(
            decoded.payload, original.payload,
            "payload must round-trip with no truncation/data-loss"
        );
    }
}

// ---------------------------------------------------------------------------
// Property
// ---------------------------------------------------------------------------

proptest! {
    /// Any sequence of frames round-trips through encode -> from_bytes with no
    /// frame dropped and no payload truncation.
    #[test]
    fn prop_war_roundtrip_preserves_all_frames(
        specs in prop::collection::vec(
            (any::<u64>(), prop::collection::vec(any::<u8>(), 0..512)),
            1..16,
        )
    ) {
        let built: Vec<RecordingFrame> = specs
            .iter()
            .map(|(ts, payload)| {
                RecordingFrame::new(*ts, FrameType::Output, 0, payload.clone())
                    .expect("payload under u32::MAX builds a valid frame")
            })
            .collect();

        let mut bytes = Vec::new();
        for frame in &built {
            bytes.extend_from_slice(&frame.encode());
        }

        let recording = Recording::from_bytes(&bytes).expect("encoded frames must decode");
        prop_assert_eq!(recording.frames.len(), built.len());
        for (decoded, original) in recording.frames.iter().zip(built.iter()) {
            prop_assert_eq!(decoded.header.timestamp_ms, original.header.timestamp_ms);
            prop_assert_eq!(&decoded.payload, &original.payload);
        }
    }

    /// `from_bytes` on arbitrary input must return `Ok`/`Err` without panicking.
    /// (proptest fails the test if the body panics.) The decoder slices into the
    /// input, so it can never allocate more than `data.len()` — no OOM regardless
    /// of any declared length prefix.
    #[test]
    fn prop_war_from_bytes_never_panics_on_arbitrary_input(
        data in prop::collection::vec(any::<u8>(), 0..2048)
    ) {
        let _ = Recording::from_bytes(&data);
    }

    /// A frame header that DECLARES more payload than is actually present must be
    /// rejected by the bounds-check (Err), NOT allocate the declared length
    /// (which can be ~4 GiB and OOM).
    #[test]
    fn prop_war_oversized_payload_len_is_rejected_not_oom(
        ts in any::<u64>(),
        declared_len in 1u32..=u32::MAX,
        actual_payload in prop::collection::vec(any::<u8>(), 0..32),
    ) {
        prop_assume!((declared_len as usize) > actual_payload.len());

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&ts.to_le_bytes()); // 8: timestamp
        bytes.push(FrameType::Output as u8);         // 1: frame_type
        bytes.push(0u8);                             // 1: flags
        bytes.extend_from_slice(&declared_len.to_le_bytes()); // 4: payload_len (oversized)
        bytes.extend_from_slice(&actual_payload);    // fewer bytes than declared

        let result = Recording::from_bytes(&bytes);
        prop_assert!(
            result.is_err(),
            "a frame declaring more payload than provided must be rejected, not allocated"
        );
    }
}
