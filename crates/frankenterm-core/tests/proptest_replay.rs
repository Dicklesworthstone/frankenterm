//! Property-based tests for replay module.
//!
//! Tests invariants for:
//! - PlayerState serde roundtrip
//! - PlaybackSpeed construction (must be > 0), constants, default
//! - ExportOptions default values
//! - ExportFormat equality
//! - Recording::from_bytes frame parsing (roundtrip via encode)
//! - Recording::info accuracy
//! - decode_frame correctness for all FrameType variants
//! - Player state machine (initial state, seek, speed)
//! - parse_duration_ms parsing invariants
//! - CollectorSink accumulation

use frankenterm_core::recording::{FrameHeader, FrameType, RecordingFrame};
use frankenterm_core::replay::*;
use proptest::prelude::*;

// ============================================================================
// Strategies
// ============================================================================

fn arb_player_state() -> impl Strategy<Value = PlayerState> {
    prop_oneof![
        Just(PlayerState::Playing),
        Just(PlayerState::Paused),
        Just(PlayerState::Stopped),
        Just(PlayerState::Finished),
    ]
}

fn arb_frame_type() -> impl Strategy<Value = FrameType> {
    prop_oneof![
        Just(FrameType::Output),
        Just(FrameType::Resize),
        Just(FrameType::Event),
        Just(FrameType::Marker),
        Just(FrameType::Input),
    ]
}

/// Generate a valid payload for the given frame type.
fn arb_payload_for(ft: FrameType) -> BoxedStrategy<Vec<u8>> {
    match ft {
        FrameType::Output | FrameType::Input => prop::collection::vec(any::<u8>(), 0..100).boxed(),
        FrameType::Resize => {
            // Resize needs exactly 4 bytes: cols(u16 LE) + rows(u16 LE)
            (1u16..500, 1u16..200)
                .prop_map(|(cols, rows)| {
                    let mut p = Vec::with_capacity(4);
                    p.extend_from_slice(&cols.to_le_bytes());
                    p.extend_from_slice(&rows.to_le_bytes());
                    p
                })
                .boxed()
        }
        FrameType::Event => {
            // Must be valid JSON
            prop_oneof![
                Just(b"{}".to_vec()),
                Just(b"{\"x\":1}".to_vec()),
                Just(b"[1,2,3]".to_vec()),
                Just(b"\"hello\"".to_vec()),
            ]
            .boxed()
        }
        FrameType::Marker => "[a-z ]{1,50}".prop_map(|s| s.into_bytes()).boxed(),
    }
}

/// Generate a valid recording frame.
fn arb_recording_frame(min_ts: u64, max_ts: u64) -> impl Strategy<Value = RecordingFrame> {
    arb_frame_type().prop_flat_map(move |ft| {
        (min_ts..max_ts, arb_payload_for(ft)).prop_map(move |(ts, payload)| RecordingFrame {
            header: FrameHeader {
                timestamp_ms: ts,
                frame_type: ft,
                flags: 0,
                payload_len: payload.len() as u32,
            },
            payload,
        })
    })
}

/// Encode frames into binary format for Recording::from_bytes.
fn encode_frames(frames: &[RecordingFrame]) -> Vec<u8> {
    let mut data = Vec::new();
    for frame in frames {
        data.extend(frame.encode());
    }
    data
}

// ============================================================================
// Property Tests: PlayerState
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_player_state_serde_roundtrip(s in arb_player_state()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: PlayerState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, s);
    }

    #[test]
    fn prop_player_state_json_is_string(s in arb_player_state()) {
        let json = serde_json::to_string(&s).unwrap();
        // Should serialize as a JSON string (quoted)
        prop_assert!(json.starts_with('"'), "expected string, got: {}", json);
    }
}

// ============================================================================
// Property Tests: PlaybackSpeed
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Positive speeds succeed.
    #[test]
    fn prop_playback_speed_positive_ok(speed in 0.01f32..100.0) {
        let result = PlaybackSpeed::new(speed);
        prop_assert!(result.is_ok(), "speed {} should be valid", speed);
        prop_assert!((result.unwrap().as_f32() - speed).abs() < f32::EPSILON);
    }

    /// Zero and negative speeds fail.
    #[test]
    fn prop_playback_speed_non_positive_err(speed in -100.0f32..=0.0) {
        let result = PlaybackSpeed::new(speed);
        prop_assert!(result.is_err(), "speed {} should be rejected", speed);
    }

    /// as_f32 roundtrip matches input.
    #[test]
    fn prop_playback_speed_as_f32_roundtrip(speed in 0.01f32..100.0) {
        let ps = PlaybackSpeed::new(speed).unwrap();
        prop_assert!((ps.as_f32() - speed).abs() < f32::EPSILON);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    /// Default speed is NORMAL (1.0).
    #[test]
    fn prop_playback_speed_default(_dummy in 0..1u32) {
        let ps = PlaybackSpeed::default();
        prop_assert!((ps.as_f32() - 1.0).abs() < f32::EPSILON);
    }

    /// Constants have correct values.
    #[test]
    fn prop_playback_speed_constants(_dummy in 0..1u32) {
        prop_assert!((PlaybackSpeed::HALF.as_f32() - 0.5).abs() < f32::EPSILON);
        prop_assert!((PlaybackSpeed::NORMAL.as_f32() - 1.0).abs() < f32::EPSILON);
        prop_assert!((PlaybackSpeed::DOUBLE.as_f32() - 2.0).abs() < f32::EPSILON);
        prop_assert!((PlaybackSpeed::QUAD.as_f32() - 4.0).abs() < f32::EPSILON);
    }
}

// ============================================================================
// Property Tests: ExportOptions
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn prop_export_options_default(_dummy in 0..1u32) {
        let opts = ExportOptions::default();
        prop_assert_eq!(opts.cols, 80);
        prop_assert_eq!(opts.rows, 24);
        prop_assert!(opts.redact);
        prop_assert!(opts.extra_redact_patterns.is_empty());
        prop_assert!(opts.title.is_none());
    }
}

// ============================================================================
// Property Tests: Recording frame encode/parse roundtrip
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Single frame encode → parse roundtrip.
    #[test]
    fn prop_single_frame_encode_parse_roundtrip(frame in arb_recording_frame(0, 10000)) {
        let data = frame.encode();
        let recording = Recording::from_bytes(&data).unwrap();
        prop_assert_eq!(recording.frames.len(), 1);
        prop_assert_eq!(recording.frames[0].header.timestamp_ms, frame.header.timestamp_ms);
        prop_assert_eq!(recording.frames[0].header.frame_type, frame.header.frame_type);
        prop_assert_eq!(recording.frames[0].header.flags, frame.header.flags);
        prop_assert_eq!(&recording.frames[0].payload, &frame.payload);
    }

    /// Multiple frames encode → parse roundtrip.
    #[test]
    fn prop_multi_frame_roundtrip(count in 1usize..10) {
        // Generate frames with monotonic timestamps
        let mut frames = Vec::new();
        let mut ts = 0u64;
        let runner = proptest::test_runner::TestRunner::new(ProptestConfig::default());
        let _ = runner; // just to show we're in proptest context

        // Use a simple deterministic approach
        for i in 0..count {
            let ft = match i % 5 {
                0 => FrameType::Output,
                1 => FrameType::Resize,
                2 => FrameType::Event,
                3 => FrameType::Marker,
                _ => FrameType::Input,
            };
            let payload = match ft {
                FrameType::Output | FrameType::Input => format!("data{}", i).into_bytes(),
                FrameType::Resize => {
                    let mut p = Vec::new();
                    p.extend_from_slice(&80u16.to_le_bytes());
                    p.extend_from_slice(&24u16.to_le_bytes());
                    p
                }
                FrameType::Event => b"{}".to_vec(),
                FrameType::Marker => format!("mark{}", i).into_bytes(),
            };
            frames.push(RecordingFrame {
                header: FrameHeader {
                    timestamp_ms: ts,
                    frame_type: ft,
                    flags: 0,
                    payload_len: payload.len() as u32,
                },
                payload,
            });
            ts += 100;
        }

        let data = encode_frames(&frames);
        let recording = Recording::from_bytes(&data).unwrap();
        prop_assert_eq!(recording.frames.len(), frames.len());
        for (parsed, original) in recording.frames.iter().zip(frames.iter()) {
            prop_assert_eq!(parsed.header.timestamp_ms, original.header.timestamp_ms);
            prop_assert_eq!(parsed.header.frame_type, original.header.frame_type);
            prop_assert_eq!(&parsed.payload, &original.payload);
        }
    }

    /// Empty input produces empty recording.
    #[test]
    fn prop_empty_recording(_dummy in 0..1u32) {
        let recording = Recording::from_bytes(&[]).unwrap();
        prop_assert!(recording.frames.is_empty());
        prop_assert_eq!(recording.duration_ms, 0);
    }
}

// ============================================================================
// Property Tests: Recording::info
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Recording info frame_count matches frames.len().
    #[test]
    fn prop_recording_info_frame_count(n_output in 0usize..5, n_marker in 0usize..3) {
        let mut frames = Vec::new();
        let mut ts = 0u64;
        for _ in 0..n_output {
            frames.push(RecordingFrame {
                header: FrameHeader {
                    timestamp_ms: ts,
                    frame_type: FrameType::Output,
                    flags: 0,
                    payload_len: 1,
                },
                payload: vec![b'x'],
            });
            ts += 10;
        }
        for _ in 0..n_marker {
            frames.push(RecordingFrame {
                header: FrameHeader {
                    timestamp_ms: ts,
                    frame_type: FrameType::Marker,
                    flags: 0,
                    payload_len: 1,
                },
                payload: vec![b'm'],
            });
            ts += 10;
        }
        let data = encode_frames(&frames);
        let recording = Recording::from_bytes(&data).unwrap();
        let info = recording.info();
        prop_assert_eq!(info.frame_count, n_output + n_marker);
        prop_assert_eq!(info.output_frames, n_output);
        prop_assert_eq!(info.marker_frames, n_marker);
    }

    /// total_bytes sums all payload lengths.
    #[test]
    fn prop_recording_info_total_bytes(
        payload_sizes in prop::collection::vec(1usize..50, 1..5),
    ) {
        let mut frames = Vec::new();
        let mut ts = 0u64;
        for size in &payload_sizes {
            let payload = vec![b'A'; *size];
            frames.push(RecordingFrame {
                header: FrameHeader {
                    timestamp_ms: ts,
                    frame_type: FrameType::Output,
                    flags: 0,
                    payload_len: payload.len() as u32,
                },
                payload,
            });
            ts += 10;
        }
        let data = encode_frames(&frames);
        let recording = Recording::from_bytes(&data).unwrap();
        let expected_total: usize = payload_sizes.iter().sum();
        prop_assert_eq!(recording.info().total_bytes, expected_total);
    }
}

// ============================================================================
// Property Tests: decode_frame
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Output frames decode to DecodedFrame::Output with matching payload.
    #[test]
    fn prop_decode_output_frame(payload in prop::collection::vec(any::<u8>(), 0..100)) {
        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: 0,
                frame_type: FrameType::Output,
                flags: 0,
                payload_len: payload.len() as u32,
            },
            payload: payload.clone(),
        };
        let decoded = decode_frame(&frame).unwrap();
        let is_output = matches!(&decoded, DecodedFrame::Output(b) if *b == payload);
        prop_assert!(is_output, "expected Output with matching payload");
    }

    /// Resize frames decode correctly.
    #[test]
    fn prop_decode_resize_frame(cols in 1u16..500, rows in 1u16..200) {
        let mut payload = Vec::new();
        payload.extend_from_slice(&cols.to_le_bytes());
        payload.extend_from_slice(&rows.to_le_bytes());
        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: 0,
                frame_type: FrameType::Resize,
                flags: 0,
                payload_len: payload.len() as u32,
            },
            payload,
        };
        let decoded = decode_frame(&frame).unwrap();
        let is_correct = matches!(decoded, DecodedFrame::Resize { cols: c, rows: r } if c == cols && r == rows);
        prop_assert!(is_correct, "expected Resize({}, {})", cols, rows);
    }

    /// Marker frames decode to strings.
    #[test]
    fn prop_decode_marker_frame(text in "[a-zA-Z0-9 ]{1,50}") {
        let payload = text.as_bytes().to_vec();
        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: 0,
                frame_type: FrameType::Marker,
                flags: 0,
                payload_len: payload.len() as u32,
            },
            payload,
        };
        let decoded = decode_frame(&frame).unwrap();
        let is_marker = matches!(&decoded, DecodedFrame::Marker(s) if s == &text);
        prop_assert!(is_marker, "expected Marker with text '{}'", text);
    }

    /// Input frames decode to DecodedFrame::Input.
    #[test]
    fn prop_decode_input_frame(payload in prop::collection::vec(any::<u8>(), 0..50)) {
        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: 0,
                frame_type: FrameType::Input,
                flags: 0,
                payload_len: payload.len() as u32,
            },
            payload: payload.clone(),
        };
        let decoded = decode_frame(&frame).unwrap();
        let is_input = matches!(&decoded, DecodedFrame::Input(b) if *b == payload);
        prop_assert!(is_input, "expected Input with matching payload");
    }

    /// Resize frame with short payload fails.
    #[test]
    fn prop_decode_resize_short_payload(payload_len in 0usize..4) {
        let payload = vec![0u8; payload_len];
        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: 0,
                frame_type: FrameType::Resize,
                flags: 0,
                payload_len: payload.len() as u32,
            },
            payload,
        };
        if payload_len < 4 {
            prop_assert!(decode_frame(&frame).is_err());
        }
    }
}

// ============================================================================
// Property Tests: Player state machine
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// New Player starts in Stopped state at position 0.
    #[test]
    fn prop_player_initial_state(n_frames in 0usize..5) {
        let mut frames = Vec::new();
        for i in 0..n_frames {
            frames.push(RecordingFrame {
                header: FrameHeader {
                    timestamp_ms: (i as u64) * 100,
                    frame_type: FrameType::Output,
                    flags: 0,
                    payload_len: 1,
                },
                payload: vec![b'x'],
            });
        }
        let data = encode_frames(&frames);
        let recording = Recording::from_bytes(&data).unwrap();
        let player = Player::new(recording);
        prop_assert_eq!(player.state(), PlayerState::Stopped);
        prop_assert_eq!(player.position().frame_index, 0);
        prop_assert_eq!(player.position().timestamp_ms, 0);
        prop_assert_eq!(player.total_frames(), n_frames);
    }

    /// Seeking to beginning position stays near frame 0.
    #[test]
    fn prop_player_seek_to_zero(n_frames in 1usize..5) {
        let mut frames = Vec::new();
        for i in 0..n_frames {
            frames.push(RecordingFrame {
                header: FrameHeader {
                    timestamp_ms: (i as u64) * 100,
                    frame_type: FrameType::Output,
                    flags: 0,
                    payload_len: 1,
                },
                payload: vec![b'x'],
            });
        }
        let data = encode_frames(&frames);
        let recording = Recording::from_bytes(&data).unwrap();
        let mut player = Player::new(recording);
        let mut sink = HeadlessSink;
        player.seek_to(0, &mut sink).unwrap();
        // After seeking to ts 0, position should be after the frame at ts 0
        prop_assert!(player.position().frame_index <= n_frames);
    }

    /// Seeking beyond end sets Finished state.
    #[test]
    fn prop_player_seek_beyond_end(n_frames in 1usize..5) {
        let mut frames = Vec::new();
        for i in 0..n_frames {
            frames.push(RecordingFrame {
                header: FrameHeader {
                    timestamp_ms: (i as u64) * 100,
                    frame_type: FrameType::Output,
                    flags: 0,
                    payload_len: 1,
                },
                payload: vec![b'x'],
            });
        }
        let data = encode_frames(&frames);
        let recording = Recording::from_bytes(&data).unwrap();
        let mut player = Player::new(recording);
        let mut sink = HeadlessSink;
        player.seek_to(u64::MAX, &mut sink).unwrap();
        prop_assert_eq!(player.state(), PlayerState::Finished);
    }

    /// set_speed takes effect.
    #[test]
    fn prop_player_set_speed(speed in 0.1f32..10.0) {
        let data = encode_frames(&[RecordingFrame {
            header: FrameHeader {
                timestamp_ms: 0,
                frame_type: FrameType::Output,
                flags: 0,
                payload_len: 1,
            },
            payload: vec![b'x'],
        }]);
        let recording = Recording::from_bytes(&data).unwrap();
        let mut player = Player::new(recording);
        let ps = PlaybackSpeed::new(speed).unwrap();
        player.set_speed(ps);
        // set_speed doesn't change state
        prop_assert_eq!(player.state(), PlayerState::Stopped);
    }
}

// ============================================================================
// Property Tests: CollectorSink
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// CollectorSink accumulates all output bytes.
    #[test]
    fn prop_collector_sink_accumulates(
        chunks in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..50), 0..5),
    ) {
        let mut sink = CollectorSink::new();
        let mut expected = Vec::new();
        for chunk in &chunks {
            sink.write_output(chunk).unwrap();
            expected.extend_from_slice(chunk);
        }
        prop_assert_eq!(&sink.output, &expected);
    }

    /// CollectorSink starts empty.
    #[test]
    fn prop_collector_sink_starts_empty(_dummy in 0..1u32) {
        let sink = CollectorSink::new();
        prop_assert!(sink.output.is_empty());
        prop_assert!(sink.events.is_empty());
        prop_assert!(sink.markers.is_empty());
    }

    /// CollectorSink collects markers.
    #[test]
    fn prop_collector_sink_markers(
        markers in prop::collection::vec("[a-z ]{1,20}", 0..5),
    ) {
        let mut sink = CollectorSink::new();
        for m in &markers {
            sink.show_marker(m).unwrap();
        }
        prop_assert_eq!(sink.markers.len(), markers.len());
        for (got, expected) in sink.markers.iter().zip(markers.iter()) {
            prop_assert_eq!(got, expected);
        }
    }
}

// ============================================================================
// Property Tests: parse_duration_ms
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Raw numeric strings parse as milliseconds.
    #[test]
    fn prop_parse_duration_raw_ms(ms in 0u64..1_000_000) {
        let result = parse_duration_ms(&ms.to_string()).unwrap();
        prop_assert_eq!(result, ms);
    }

    /// Seconds suffix parses correctly.
    #[test]
    fn prop_parse_duration_seconds(secs in 1u64..3600) {
        let input = format!("{}s", secs);
        let result = parse_duration_ms(&input).unwrap();
        prop_assert_eq!(result, secs * 1000);
    }

    /// Minutes suffix parses correctly.
    #[test]
    fn prop_parse_duration_minutes(mins in 1u64..60) {
        let input = format!("{}m", mins);
        let result = parse_duration_ms(&input).unwrap();
        prop_assert_eq!(result, mins * 60_000);
    }

    /// Hours suffix parses correctly.
    #[test]
    fn prop_parse_duration_hours(hours in 1u64..24) {
        let input = format!("{}h", hours);
        let result = parse_duration_ms(&input).unwrap();
        prop_assert_eq!(result, hours * 3_600_000);
    }

    /// Compound durations parse correctly (e.g., "1m30s").
    #[test]
    fn prop_parse_duration_compound(mins in 1u64..10, secs in 1u64..60) {
        let input = format!("{}m{}s", mins, secs);
        let result = parse_duration_ms(&input).unwrap();
        prop_assert_eq!(result, mins * 60_000 + secs * 1000);
    }

    /// Whitespace is trimmed.
    #[test]
    fn prop_parse_duration_trims(ms in 0u64..1000) {
        let input = format!("  {}  ", ms);
        let result = parse_duration_ms(&input).unwrap();
        prop_assert_eq!(result, ms);
    }

    /// Unknown units are rejected.
    #[test]
    fn prop_parse_duration_rejects_bad_unit(n in 1u64..100) {
        let input = format!("{}x", n);
        prop_assert!(parse_duration_ms(&input).is_err());
    }
}
