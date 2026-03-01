//! LabRuntime-ported replay tests for deterministic async testing.
//!
//! Ports `#[tokio::test]` async functions from `replay.rs` to asupersync-based
//! `RuntimeFixture`, gaining seed-based reproducibility for playback control,
//! event routing, and position tracking tests.
//!
//! Tests with `#[cfg(not(feature = "asupersync-runtime"))]` are intentionally
//! omitted because they rely on `tokio::time::pause()` which is not available
//! under asupersync-runtime.
//!
//! Bead: ft-22x4r

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;

use frankenterm_core::recording::{FrameHeader, FrameType, RecordingFrame};
use frankenterm_core::replay::{
    CollectorSink, HeadlessSink, PlaybackSpeed, Player, PlayerControl, PlayerState, Recording,
};
use frankenterm_core::runtime_compat::watch;

// ---------------------------------------------------------------------------
// Helpers (mirrors private helpers from replay.rs tests module)
// ---------------------------------------------------------------------------

/// Build a test recording from frame specs: (timestamp_ms, frame_type, payload).
///
/// This reimplements the private `build_recording` helper from the in-module
/// tests, encoding each frame via `RecordingFrame::encode()` and concatenating
/// the raw bytes.
fn build_recording(specs: &[(u64, FrameType, Vec<u8>)]) -> Vec<u8> {
    let mut data = Vec::new();
    for (ts, ft, payload) in specs {
        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: *ts,
                frame_type: *ft,
                flags: 0,
                payload_len: payload.len() as u32,
            },
            payload: payload.clone(),
        };
        data.extend(frame.encode());
    }
    data
}

// ===========================================================================
// Ported async tests
// ===========================================================================

#[test]
fn play_simple_all_frames() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let data = build_recording(&[
            (0, FrameType::Output, b"A".to_vec()),
            (10, FrameType::Output, b"B".to_vec()),
            (20, FrameType::Marker, b"done".to_vec()),
        ]);
        let recording = Recording::from_bytes(&data).unwrap();
        let mut player = Player::new(recording);
        player.set_speed(PlaybackSpeed::QUAD); // fast
        let mut sink = CollectorSink::new();

        player.play_simple(&mut sink).await.unwrap();

        assert_eq!(player.state(), PlayerState::Finished);
        assert_eq!(sink.output, b"AB");
        assert_eq!(sink.markers, vec!["done"]);
    });
}

#[test]
fn play_with_stop_control() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        // Send stop before play starts to deterministically test the pre-loop
        // control-check path.
        let data = build_recording(&[
            (0, FrameType::Output, b"A".to_vec()),
            (5000, FrameType::Output, b"B".to_vec()),
            (10000, FrameType::Output, b"C".to_vec()),
        ]);
        let recording = Recording::from_bytes(&data).unwrap();
        let mut player = Player::new(recording);

        let (tx, rx) = watch::channel(PlayerControl::Play);
        let mut sink = CollectorSink::new();

        // Send stop before play -- the very first check_control sees it.
        let _ = tx.send(PlayerControl::Stop);

        player.play(&mut sink, rx).await.unwrap();
        assert_eq!(player.state(), PlayerState::Stopped);
        // Stop arrived before any frames were output.
        assert!(sink.output.is_empty());
    });
}

#[test]
fn play_with_events_and_markers() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let event_payload = serde_json::to_vec(&serde_json::json!({"rule": "test"})).unwrap();
        let data = build_recording(&[
            (0, FrameType::Output, b"text".to_vec()),
            (50, FrameType::Event, event_payload),
            (100, FrameType::Marker, b"annotation".to_vec()),
            (150, FrameType::Output, b"more".to_vec()),
        ]);
        let recording = Recording::from_bytes(&data).unwrap();
        let mut player = Player::new(recording);
        player.set_speed(PlaybackSpeed::QUAD);
        let mut sink = CollectorSink::new();

        player.play_simple(&mut sink).await.unwrap();

        assert_eq!(sink.output, b"textmore");
        assert_eq!(sink.events.len(), 1);
        assert_eq!(sink.events[0]["rule"], "test");
        assert_eq!(sink.markers, vec!["annotation"]);
    });
}

#[test]
fn play_empty_recording_finishes_immediately() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let recording = Recording::from_bytes(&[]).unwrap();
        let mut player = Player::new(recording);
        let mut sink = HeadlessSink;

        player.play_simple(&mut sink).await.unwrap();
        assert_eq!(player.state(), PlayerState::Finished);
    });
}

#[test]
fn play_simple_records_correct_final_position() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let data = build_recording(&[
            (0, FrameType::Output, b"a".to_vec()),
            (100, FrameType::Output, b"b".to_vec()),
            (500, FrameType::Output, b"c".to_vec()),
        ]);
        let recording = Recording::from_bytes(&data).unwrap();
        let mut player = Player::new(recording);
        player.set_speed(PlaybackSpeed::QUAD);
        let mut sink = CollectorSink::new();

        player.play_simple(&mut sink).await.unwrap();

        assert_eq!(player.position().frame_index, 3);
        assert_eq!(sink.output, b"abc");
    });
}
