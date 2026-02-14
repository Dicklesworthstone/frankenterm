//! Property-based tests for the semantic chunking module.
//!
//! Verifies `build_semantic_chunks` invariants including:
//! - Empty / trivial input handling
//! - Determinism and input-order independence
//! - Chunk metadata correctness (policy_version, chunk_id, content_hash)
//! - Offset ordering within and across chunks
//! - Config limit enforcement (max_chunk_chars, max_chunk_events)
//! - Direction correctness for ingress-only and egress-only streams
//! - Hard gap boundary enforcement (time gap, pane_id separation)
//! - ChunkPolicyConfig / ChunkDirection serde roundtrips
//! - Overlap metadata consistency
//! - Content hash and chunk_id determinism

use proptest::collection::vec as arb_vec;
use proptest::prelude::*;

use frankenterm_core::recorder_storage::RecorderOffset;
use frankenterm_core::recording::{
    RecorderControlMarkerType, RecorderEvent, RecorderEventCausality, RecorderEventPayload,
    RecorderEventSource, RecorderIngressKind, RecorderLifecyclePhase, RecorderRedactionLevel,
    RecorderSegmentKind, RecorderTextEncoding,
};
use frankenterm_core::search::{
    ChunkDirection, ChunkInputEvent, ChunkPolicyConfig, RECORDER_CHUNKING_POLICY_V1, SemanticChunk,
    build_semantic_chunks,
};

// ────────────────────────────────────────────────────────────────────
// Helper: build RecorderEvent instances
// ────────────────────────────────────────────────────────────────────

fn make_causality() -> RecorderEventCausality {
    RecorderEventCausality {
        parent_event_id: None,
        trigger_event_id: None,
        root_event_id: None,
    }
}

fn make_ingress_event(
    event_id: &str,
    pane_id: u64,
    text: &str,
    occurred_at_ms: u64,
    sequence: u64,
) -> RecorderEvent {
    RecorderEvent {
        schema_version: "ft.recorder.event.v1".to_string(),
        event_id: event_id.to_string(),
        pane_id,
        session_id: Some("test-session".to_string()),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::WeztermMux,
        occurred_at_ms,
        recorded_at_ms: occurred_at_ms + 1,
        sequence,
        causality: make_causality(),
        payload: RecorderEventPayload::IngressText {
            text: text.to_string(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            ingress_kind: RecorderIngressKind::SendText,
        },
    }
}

fn make_egress_event(
    event_id: &str,
    pane_id: u64,
    text: &str,
    occurred_at_ms: u64,
    sequence: u64,
) -> RecorderEvent {
    RecorderEvent {
        schema_version: "ft.recorder.event.v1".to_string(),
        event_id: event_id.to_string(),
        pane_id,
        session_id: Some("test-session".to_string()),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::WeztermMux,
        occurred_at_ms,
        recorded_at_ms: occurred_at_ms + 1,
        sequence,
        causality: make_causality(),
        payload: RecorderEventPayload::EgressOutput {
            text: text.to_string(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            segment_kind: RecorderSegmentKind::Delta,
            is_gap: false,
        },
    }
}

fn make_control_marker_event(
    event_id: &str,
    pane_id: u64,
    occurred_at_ms: u64,
    sequence: u64,
) -> RecorderEvent {
    RecorderEvent {
        schema_version: "ft.recorder.event.v1".to_string(),
        event_id: event_id.to_string(),
        pane_id,
        session_id: Some("test-session".to_string()),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::WeztermMux,
        occurred_at_ms,
        recorded_at_ms: occurred_at_ms + 1,
        sequence,
        causality: make_causality(),
        payload: RecorderEventPayload::ControlMarker {
            control_marker_type: RecorderControlMarkerType::PromptBoundary,
            details: serde_json::Value::Null,
        },
    }
}

fn make_lifecycle_marker_event(
    event_id: &str,
    pane_id: u64,
    occurred_at_ms: u64,
    sequence: u64,
) -> RecorderEvent {
    RecorderEvent {
        schema_version: "ft.recorder.event.v1".to_string(),
        event_id: event_id.to_string(),
        pane_id,
        session_id: Some("test-session".to_string()),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::WeztermMux,
        occurred_at_ms,
        recorded_at_ms: occurred_at_ms + 1,
        sequence,
        causality: make_causality(),
        payload: RecorderEventPayload::LifecycleMarker {
            lifecycle_phase: RecorderLifecyclePhase::CaptureStarted,
            reason: None,
            details: serde_json::Value::Null,
        },
    }
}

fn make_offset(segment_id: u64, ordinal: u64, byte_offset: u64) -> RecorderOffset {
    RecorderOffset {
        segment_id,
        byte_offset,
        ordinal,
    }
}

fn make_chunk_input(event: RecorderEvent, offset: RecorderOffset) -> ChunkInputEvent {
    ChunkInputEvent { event, offset }
}

// ────────────────────────────────────────────────────────────────────
// Proptest strategies
// ────────────────────────────────────────────────────────────────────

/// Generate short ASCII text strings (1-200 chars).
fn arb_text() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 .,;:!?\\-_]{1,200}"
}

/// Generate a pane ID in a small range.
fn arb_pane_id() -> impl Strategy<Value = u64> {
    1u64..=10
}

/// Generate a valid ChunkPolicyConfig with reasonable parameter ranges.
fn arb_chunk_policy_config() -> impl Strategy<Value = ChunkPolicyConfig> {
    (
        200usize..=4000,     // max_chunk_chars
        4usize..=100,        // max_chunk_events
        10_000u64..=300_000, // max_window_ms
        5_000u64..=60_000,   // hard_gap_ms
        10usize..=200,       // min_chunk_chars
        1_000u64..=30_000,   // merge_window_ms
        0usize..=300,        // overlap_chars
    )
        .prop_map(
            |(
                max_chunk_chars,
                max_chunk_events,
                max_window_ms,
                hard_gap_ms,
                min_chunk_chars,
                merge_window_ms,
                overlap_chars,
            )| {
                ChunkPolicyConfig {
                    max_chunk_chars,
                    max_chunk_events,
                    max_window_ms,
                    hard_gap_ms,
                    min_chunk_chars,
                    merge_window_ms,
                    overlap_chars,
                }
            },
        )
}

/// Build a RecorderEvent that is either IngressText or EgressOutput.
#[allow(dead_code)]
fn arb_recorder_event(
    pane_id: u64,
    occurred_at_ms: u64,
    sequence: u64,
) -> impl Strategy<Value = RecorderEvent> {
    (arb_text(), prop::bool::ANY).prop_map(move |(text, is_ingress)| {
        if is_ingress {
            make_ingress_event(
                &format!("evt-{}-{}", pane_id, sequence),
                pane_id,
                &text,
                occurred_at_ms,
                sequence,
            )
        } else {
            make_egress_event(
                &format!("evt-{}-{}", pane_id, sequence),
                pane_id,
                &text,
                occurred_at_ms,
                sequence,
            )
        }
    })
}

/// Generate a Vec of ChunkInputEvents with consistent ordering on a single pane.
fn arb_chunk_input_events_single_pane(
    n: std::ops::RangeInclusive<usize>,
) -> impl Strategy<Value = Vec<ChunkInputEvent>> {
    arb_pane_id().prop_flat_map(move |pane_id| {
        arb_vec(arb_text().prop_flat_map(|_| prop::bool::ANY), n.clone()).prop_flat_map(
            move |directions| {
                let count = directions.len();
                let pane = pane_id;
                arb_vec(arb_text(), count..=count).prop_map(move |texts| {
                    texts
                        .into_iter()
                        .enumerate()
                        .map(|(i, text)| {
                            let seq = i as u64;
                            let ts = 1000 + (i as u64) * 500;
                            let event = if directions[i] {
                                make_ingress_event(
                                    &format!("evt-{}-{}", pane, seq),
                                    pane,
                                    &text,
                                    ts,
                                    seq,
                                )
                            } else {
                                make_egress_event(
                                    &format!("evt-{}-{}", pane, seq),
                                    pane,
                                    &text,
                                    ts,
                                    seq,
                                )
                            };
                            let offset = make_offset(1, seq, seq * 256);
                            make_chunk_input(event, offset)
                        })
                        .collect()
                })
            },
        )
    })
}

/// Generate ingress-only ChunkInputEvents.
fn arb_ingress_only_events(
    n: std::ops::RangeInclusive<usize>,
) -> impl Strategy<Value = Vec<ChunkInputEvent>> {
    arb_pane_id().prop_flat_map(move |pane_id| {
        arb_vec(arb_text(), n.clone()).prop_map(move |texts| {
            texts
                .into_iter()
                .enumerate()
                .map(|(i, text)| {
                    let seq = i as u64;
                    let ts = 1000 + (i as u64) * 500;
                    let event = make_ingress_event(
                        &format!("evt-{}-{}", pane_id, seq),
                        pane_id,
                        &text,
                        ts,
                        seq,
                    );
                    let offset = make_offset(1, seq, seq * 256);
                    make_chunk_input(event, offset)
                })
                .collect()
        })
    })
}

/// Generate egress-only ChunkInputEvents.
fn arb_egress_only_events(
    n: std::ops::RangeInclusive<usize>,
) -> impl Strategy<Value = Vec<ChunkInputEvent>> {
    arb_pane_id().prop_flat_map(move |pane_id| {
        arb_vec(arb_text(), n.clone()).prop_map(move |texts| {
            texts
                .into_iter()
                .enumerate()
                .map(|(i, text)| {
                    let seq = i as u64;
                    let ts = 1000 + (i as u64) * 500;
                    let event = make_egress_event(
                        &format!("evt-{}-{}", pane_id, seq),
                        pane_id,
                        &text,
                        ts,
                        seq,
                    );
                    let offset = make_offset(1, seq, seq * 256);
                    make_chunk_input(event, offset)
                })
                .collect()
        })
    })
}

// ────────────────────────────────────────────────────────────────────
// Group 1: Empty / trivial input
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// 1. Empty input always produces empty output.
    #[test]
    fn empty_input_produces_empty_output(
        config in arb_chunk_policy_config(),
    ) {
        let result = build_semantic_chunks(&[], &config);
        prop_assert!(result.is_empty(), "expected empty output for empty input");
    }

    /// 2. Single ingress event produces exactly one chunk.
    #[test]
    fn single_ingress_event_one_chunk(
        text in arb_text(),
        pane_id in arb_pane_id(),
    ) {
        let event = make_ingress_event("evt-1", pane_id, &text, 1000, 0);
        let offset = make_offset(1, 0, 0);
        let inputs = vec![make_chunk_input(event, offset)];
        let config = ChunkPolicyConfig::default();
        let chunks = build_semantic_chunks(&inputs, &config);
        prop_assert_eq!(chunks.len(), 1, "expected exactly 1 chunk for single ingress");
    }

    /// 3. Single egress event produces exactly one chunk.
    #[test]
    fn single_egress_event_one_chunk(
        text in arb_text(),
        pane_id in arb_pane_id(),
    ) {
        let event = make_egress_event("evt-1", pane_id, &text, 1000, 0);
        let offset = make_offset(1, 0, 0);
        let inputs = vec![make_chunk_input(event, offset)];
        let config = ChunkPolicyConfig::default();
        let chunks = build_semantic_chunks(&inputs, &config);
        prop_assert_eq!(chunks.len(), 1, "expected exactly 1 chunk for single egress");
    }

    /// 4. Only ControlMarker / LifecycleMarker events produce no chunks.
    #[test]
    fn boundary_only_events_produce_no_chunks(
        pane_id in arb_pane_id(),
        n in 1usize..=10,
    ) {
        let inputs: Vec<ChunkInputEvent> = (0..n)
            .map(|i| {
                let seq = i as u64;
                let event = if i % 2 == 0 {
                    make_control_marker_event(
                        &format!("ctl-{}", seq),
                        pane_id,
                        1000 + seq * 100,
                        seq,
                    )
                } else {
                    make_lifecycle_marker_event(
                        &format!("lc-{}", seq),
                        pane_id,
                        1000 + seq * 100,
                        seq,
                    )
                };
                make_chunk_input(event, make_offset(1, seq, seq * 64))
            })
            .collect();

        let config = ChunkPolicyConfig::default();
        let chunks = build_semantic_chunks(&inputs, &config);
        prop_assert!(
            chunks.is_empty(),
            "boundary-only events should produce no chunks, got {}",
            chunks.len()
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Group 2: Determinism
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// 5. Same input produces same output (determinism).
    #[test]
    fn deterministic_same_input_same_output(
        inputs in arb_chunk_input_events_single_pane(2..=20),
        config in arb_chunk_policy_config(),
    ) {
        let result1 = build_semantic_chunks(&inputs, &config);
        let result2 = build_semantic_chunks(&inputs, &config);
        prop_assert_eq!(result1.len(), result2.len(), "chunk count differs across calls");
        for (i, (a, b)) in result1.iter().zip(result2.iter()).enumerate() {
            prop_assert_eq!(
                &a.chunk_id, &b.chunk_id,
                "chunk_id mismatch at index {}", i
            );
            prop_assert_eq!(
                &a.text, &b.text,
                "text mismatch at index {}", i
            );
            prop_assert_eq!(
                &a.content_hash, &b.content_hash,
                "content_hash mismatch at index {}", i
            );
        }
    }

    /// 6. Shuffled input order produces same output (sorted internally).
    #[test]
    fn shuffled_input_same_output(
        inputs in arb_chunk_input_events_single_pane(2..=15),
    ) {
        let config = ChunkPolicyConfig::default();
        let result_ordered = build_semantic_chunks(&inputs, &config);

        // Reverse the input order.
        let mut reversed = inputs.clone();
        reversed.reverse();
        let result_reversed = build_semantic_chunks(&reversed, &config);

        prop_assert_eq!(
            result_ordered.len(), result_reversed.len(),
            "chunk count differs: ordered={} reversed={}",
            result_ordered.len(), result_reversed.len()
        );
        for (i, (a, b)) in result_ordered.iter().zip(result_reversed.iter()).enumerate() {
            prop_assert_eq!(
                &a.chunk_id, &b.chunk_id,
                "chunk_id mismatch at index {} after shuffle", i
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Group 3: Chunk metadata invariants
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    /// 7. Every chunk has policy_version == RECORDER_CHUNKING_POLICY_V1.
    #[test]
    fn policy_version_correct(
        inputs in arb_chunk_input_events_single_pane(1..=20),
    ) {
        let config = ChunkPolicyConfig::default();
        let chunks = build_semantic_chunks(&inputs, &config);
        for (i, chunk) in chunks.iter().enumerate() {
            prop_assert_eq!(
                chunk.policy_version.as_str(),
                RECORDER_CHUNKING_POLICY_V1,
                "policy_version mismatch at chunk {}", i
            );
        }
    }

    /// 8. Every chunk has a non-empty chunk_id that is a SHA-256 hex string (64 hex chars).
    #[test]
    fn chunk_id_is_sha256_hex(
        inputs in arb_chunk_input_events_single_pane(1..=20),
    ) {
        let config = ChunkPolicyConfig::default();
        let chunks = build_semantic_chunks(&inputs, &config);
        for (i, chunk) in chunks.iter().enumerate() {
            prop_assert!(
                !chunk.chunk_id.is_empty(),
                "chunk_id is empty at chunk {}", i
            );
            prop_assert_eq!(
                chunk.chunk_id.len(), 64,
                "chunk_id not 64 hex chars at chunk {}: len={}", i, chunk.chunk_id.len()
            );
            prop_assert!(
                chunk.chunk_id.chars().all(|c| c.is_ascii_hexdigit()),
                "chunk_id has non-hex chars at chunk {}", i
            );
        }
    }

    /// 9. Every chunk has a non-empty content_hash (64 hex chars).
    #[test]
    fn content_hash_is_sha256_hex(
        inputs in arb_chunk_input_events_single_pane(1..=20),
    ) {
        let config = ChunkPolicyConfig::default();
        let chunks = build_semantic_chunks(&inputs, &config);
        for (i, chunk) in chunks.iter().enumerate() {
            prop_assert!(
                !chunk.content_hash.is_empty(),
                "content_hash is empty at chunk {}", i
            );
            prop_assert_eq!(
                chunk.content_hash.len(), 64,
                "content_hash not 64 hex chars at chunk {}: len={}", i, chunk.content_hash.len()
            );
            prop_assert!(
                chunk.content_hash.chars().all(|c| c.is_ascii_hexdigit()),
                "content_hash has non-hex chars at chunk {}", i
            );
        }
    }

    /// 10. text_chars matches the actual char count of the chunk text.
    #[test]
    fn text_chars_matches_actual(
        inputs in arb_chunk_input_events_single_pane(1..=20),
    ) {
        let config = ChunkPolicyConfig::default();
        let chunks = build_semantic_chunks(&inputs, &config);
        for (i, chunk) in chunks.iter().enumerate() {
            let actual_chars = chunk.text.chars().count();
            prop_assert_eq!(
                chunk.text_chars, actual_chars,
                "text_chars mismatch at chunk {}: field={} actual={}", i, chunk.text_chars, actual_chars
            );
        }
    }

    /// 11. event_count equals event_ids.len().
    #[test]
    fn event_count_matches_event_ids_len(
        inputs in arb_chunk_input_events_single_pane(1..=20),
    ) {
        let config = ChunkPolicyConfig::default();
        let chunks = build_semantic_chunks(&inputs, &config);
        for (i, chunk) in chunks.iter().enumerate() {
            prop_assert_eq!(
                chunk.event_count,
                chunk.event_ids.len(),
                "event_count != event_ids.len() at chunk {}: count={} ids={}",
                i, chunk.event_count, chunk.event_ids.len()
            );
        }
    }

    /// 12. occurred_at_start_ms <= occurred_at_end_ms in every chunk.
    #[test]
    fn timestamp_ordering(
        inputs in arb_chunk_input_events_single_pane(1..=20),
    ) {
        let config = ChunkPolicyConfig::default();
        let chunks = build_semantic_chunks(&inputs, &config);
        for (i, chunk) in chunks.iter().enumerate() {
            prop_assert!(
                chunk.occurred_at_start_ms <= chunk.occurred_at_end_ms,
                "start > end at chunk {}: start={} end={}",
                i, chunk.occurred_at_start_ms, chunk.occurred_at_end_ms
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Group 4: Offset ordering
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    /// 13. Within each chunk: start_offset.ordinal <= end_offset.ordinal.
    #[test]
    fn start_offset_lte_end_offset(
        inputs in arb_chunk_input_events_single_pane(1..=20),
    ) {
        let config = ChunkPolicyConfig::default();
        let chunks = build_semantic_chunks(&inputs, &config);
        for (i, chunk) in chunks.iter().enumerate() {
            prop_assert!(
                chunk.start_offset.ordinal <= chunk.end_offset.ordinal,
                "start_offset.ordinal > end_offset.ordinal at chunk {}: start={} end={}",
                i, chunk.start_offset.ordinal, chunk.end_offset.ordinal
            );
        }
    }

    /// 14. Chunks are output in offset order (start_offset non-decreasing).
    #[test]
    fn chunks_in_offset_order(
        inputs in arb_chunk_input_events_single_pane(2..=20),
    ) {
        let config = ChunkPolicyConfig::default();
        let chunks = build_semantic_chunks(&inputs, &config);
        for window in chunks.windows(2) {
            let (a, b) = (&window[0], &window[1]);
            let a_key = (a.start_offset.segment_id, a.start_offset.ordinal);
            let b_key = (b.start_offset.segment_id, b.start_offset.ordinal);
            prop_assert!(
                a_key <= b_key,
                "chunks not in offset order: {:?} > {:?}", a_key, b_key
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Group 5: Config limit enforcement
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    /// 15. No chunk exceeds max_chunk_events.
    #[test]
    fn no_chunk_exceeds_max_events(
        inputs in arb_chunk_input_events_single_pane(1..=30),
        config in arb_chunk_policy_config(),
    ) {
        let chunks = build_semantic_chunks(&inputs, &config);
        for (i, chunk) in chunks.iter().enumerate() {
            // After glue rules merge, the event count can be up to 2x max_chunk_events
            // in worst case (two chunks merged). But single pre-glue chunks respect
            // the limit. Post-glue can merge tiny fragments; we check a looser bound.
            // The implementation allows merging up to 2 chunks during glue, so the
            // effective limit is 2 * max_chunk_events.
            let effective_limit = config.max_chunk_events * 2;
            prop_assert!(
                chunk.event_count <= effective_limit,
                "event_count {} exceeds 2*max_chunk_events {} at chunk {}",
                chunk.event_count, effective_limit, i
            );
        }
    }

    /// 16. No chunk exceeds max_chunk_chars by more than a generous factor
    /// (glue can merge two chunks, and overlap adds chars).
    #[test]
    fn no_chunk_exceeds_char_limit_with_glue(
        inputs in arb_chunk_input_events_single_pane(1..=30),
        config in arb_chunk_policy_config(),
    ) {
        let chunks = build_semantic_chunks(&inputs, &config);
        // After glue, two chunks can be merged. Each pre-glue chunk is at most
        // max_chunk_chars + overlap_chars. Glue can combine two such chunks with
        // a separator newline. We allow 2*(max_chunk_chars + overlap_chars) + 1.
        let glue_limit = 2 * (config.max_chunk_chars + config.overlap_chars) + 1;
        for (i, chunk) in chunks.iter().enumerate() {
            prop_assert!(
                chunk.text_chars <= glue_limit,
                "text_chars {} exceeds glue limit {} at chunk {}",
                chunk.text_chars, glue_limit, i
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Group 6: Direction correctness
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// 17. Ingress-only input produces chunks with Ingress or MixedGlued direction.
    #[test]
    fn ingress_only_direction(
        inputs in arb_ingress_only_events(1..=15),
    ) {
        let config = ChunkPolicyConfig::default();
        let chunks = build_semantic_chunks(&inputs, &config);
        for (i, chunk) in chunks.iter().enumerate() {
            prop_assert!(
                chunk.direction == ChunkDirection::Ingress
                    || chunk.direction == ChunkDirection::MixedGlued,
                "ingress-only input produced {:?} at chunk {}", chunk.direction, i
            );
        }
    }

    /// 18. Egress-only input produces chunks with Egress or MixedGlued direction.
    #[test]
    fn egress_only_direction(
        inputs in arb_egress_only_events(1..=15),
    ) {
        let config = ChunkPolicyConfig::default();
        let chunks = build_semantic_chunks(&inputs, &config);
        for (i, chunk) in chunks.iter().enumerate() {
            prop_assert!(
                chunk.direction == ChunkDirection::Egress
                    || chunk.direction == ChunkDirection::MixedGlued,
                "egress-only input produced {:?} at chunk {}", chunk.direction, i
            );
        }
    }

    /// 19. Direction is always one of Ingress, Egress, or MixedGlued.
    #[test]
    fn direction_is_valid_variant(
        inputs in arb_chunk_input_events_single_pane(1..=20),
    ) {
        let config = ChunkPolicyConfig::default();
        let chunks = build_semantic_chunks(&inputs, &config);
        for (i, chunk) in chunks.iter().enumerate() {
            prop_assert!(
                matches!(
                    chunk.direction,
                    ChunkDirection::Ingress | ChunkDirection::Egress | ChunkDirection::MixedGlued
                ),
                "invalid direction {:?} at chunk {}", chunk.direction, i
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Group 7: Hard gap boundary
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// 20. Events with time gap > hard_gap_ms are never in the same chunk.
    ///
    /// We construct two groups of events separated by a massive time gap and
    /// verify they end up in different chunks.
    #[test]
    fn hard_time_gap_splits_chunks(
        text1 in arb_text(),
        text2 in arb_text(),
        pane_id in arb_pane_id(),
    ) {
        let config = ChunkPolicyConfig::default();
        let gap = config.hard_gap_ms + 1;

        let e1 = make_egress_event("evt-1", pane_id, &text1, 1000, 0);
        let e2 = make_egress_event("evt-2", pane_id, &text2, 1000 + gap, 1);
        let inputs = vec![
            make_chunk_input(e1, make_offset(1, 0, 0)),
            make_chunk_input(e2, make_offset(1, 1, 256)),
        ];

        let chunks = build_semantic_chunks(&inputs, &config);
        prop_assert!(
            chunks.len() >= 2,
            "expected at least 2 chunks due to hard gap, got {}", chunks.len()
        );
        // Verify no single chunk spans both timestamps.
        for (i, chunk) in chunks.iter().enumerate() {
            let span_ms = chunk.occurred_at_end_ms.saturating_sub(chunk.occurred_at_start_ms);
            prop_assert!(
                span_ms <= config.hard_gap_ms,
                "chunk {} spans {}ms which exceeds hard_gap_ms {}",
                i, span_ms, config.hard_gap_ms
            );
        }
    }

    /// 21. Events with different pane_id are never in the same chunk.
    #[test]
    fn different_pane_id_splits_chunks(
        text1 in arb_text(),
        text2 in arb_text(),
    ) {
        let config = ChunkPolicyConfig::default();
        let e1 = make_egress_event("evt-1", 1, &text1, 1000, 0);
        let e2 = make_egress_event("evt-2", 2, &text2, 1001, 1);
        let inputs = vec![
            make_chunk_input(e1, make_offset(1, 0, 0)),
            make_chunk_input(e2, make_offset(1, 1, 256)),
        ];

        let chunks = build_semantic_chunks(&inputs, &config);
        prop_assert!(
            chunks.len() >= 2,
            "expected at least 2 chunks for different pane_ids, got {}", chunks.len()
        );
        // Each chunk should have a single pane_id.
        for (i, chunk) in chunks.iter().enumerate() {
            prop_assert!(
                chunk.pane_id == 1 || chunk.pane_id == 2,
                "unexpected pane_id {} at chunk {}", chunk.pane_id, i
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Group 8: ChunkPolicyConfig serialization
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// 22. Serde JSON roundtrip preserves ChunkPolicyConfig.
    #[test]
    fn chunk_policy_config_serde_roundtrip(
        config in arb_chunk_policy_config(),
    ) {
        let json = serde_json::to_string(&config).expect("serialize config");
        let deserialized: ChunkPolicyConfig =
            serde_json::from_str(&json).expect("deserialize config");
        prop_assert_eq!(config, deserialized, "serde roundtrip mismatch");
    }

    /// 23. Default config has all non-zero values.
    #[test]
    fn default_config_all_nonzero(_dummy in 0u8..1) {
        let config = ChunkPolicyConfig::default();
        prop_assert!(config.max_chunk_chars > 0, "max_chunk_chars is zero");
        prop_assert!(config.max_chunk_events > 0, "max_chunk_events is zero");
        prop_assert!(config.max_window_ms > 0, "max_window_ms is zero");
        prop_assert!(config.hard_gap_ms > 0, "hard_gap_ms is zero");
        prop_assert!(config.min_chunk_chars > 0, "min_chunk_chars is zero");
        prop_assert!(config.merge_window_ms > 0, "merge_window_ms is zero");
        prop_assert!(config.overlap_chars > 0, "overlap_chars is zero");
    }
}

// ────────────────────────────────────────────────────────────────────
// Group 9: ChunkDirection serde
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// 24. ChunkDirection JSON roundtrip for all variants.
    #[test]
    fn chunk_direction_serde_roundtrip(
        variant in prop_oneof![
            Just(ChunkDirection::Ingress),
            Just(ChunkDirection::Egress),
            Just(ChunkDirection::MixedGlued),
        ],
    ) {
        let json = serde_json::to_string(&variant).expect("serialize direction");
        let deserialized: ChunkDirection =
            serde_json::from_str(&json).expect("deserialize direction");
        prop_assert_eq!(variant, deserialized, "direction serde roundtrip mismatch");
    }
}

// ────────────────────────────────────────────────────────────────────
// Group 10: Overlap
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// 25. If overlap exists, overlap.chars == overlap.text.chars().count().
    #[test]
    fn overlap_chars_match_text(
        inputs in arb_chunk_input_events_single_pane(2..=20),
    ) {
        let config = ChunkPolicyConfig::default();
        let chunks = build_semantic_chunks(&inputs, &config);
        for (i, chunk) in chunks.iter().enumerate() {
            if let Some(ref overlap) = chunk.overlap {
                let actual_chars = overlap.text.chars().count();
                prop_assert_eq!(
                    overlap.chars, actual_chars,
                    "overlap.chars mismatch at chunk {}: field={} actual={}",
                    i, overlap.chars, actual_chars
                );
            }
        }
    }

    /// 26. If overlap exists, overlap.from_chunk_id is non-empty.
    #[test]
    fn overlap_from_chunk_id_nonempty(
        inputs in arb_chunk_input_events_single_pane(2..=20),
    ) {
        let config = ChunkPolicyConfig::default();
        let chunks = build_semantic_chunks(&inputs, &config);
        for (i, chunk) in chunks.iter().enumerate() {
            if let Some(ref overlap) = chunk.overlap {
                prop_assert!(
                    !overlap.from_chunk_id.is_empty(),
                    "overlap.from_chunk_id is empty at chunk {}", i
                );
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Group 11: Content hash determinism
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// 27. Same text content in identical events produces same content_hash.
    #[test]
    fn same_text_same_content_hash(
        text in arb_text(),
        pane_id in arb_pane_id(),
    ) {
        let e1 = make_egress_event("evt-1", pane_id, &text, 1000, 0);
        let e2 = make_egress_event("evt-1", pane_id, &text, 1000, 0);
        let input1 = vec![make_chunk_input(e1, make_offset(1, 0, 0))];
        let input2 = vec![make_chunk_input(e2, make_offset(1, 0, 0))];
        let config = ChunkPolicyConfig::default();

        let chunks1 = build_semantic_chunks(&input1, &config);
        let chunks2 = build_semantic_chunks(&input2, &config);

        prop_assert_eq!(chunks1.len(), 1, "expected 1 chunk from input1");
        prop_assert_eq!(chunks2.len(), 1, "expected 1 chunk from input2");
        prop_assert_eq!(
            &chunks1[0].content_hash, &chunks2[0].content_hash,
            "content_hash differs for identical text"
        );
    }

    /// 28. chunk_id is deterministic given identical inputs.
    #[test]
    fn chunk_id_deterministic(
        text in arb_text(),
        pane_id in arb_pane_id(),
    ) {
        let config = ChunkPolicyConfig::default();

        let mk = || {
            let e = make_ingress_event("evt-stable", pane_id, &text, 5000, 0);
            vec![make_chunk_input(e, make_offset(1, 0, 0))]
        };

        let chunks_a = build_semantic_chunks(&mk(), &config);
        let chunks_b = build_semantic_chunks(&mk(), &config);

        prop_assert_eq!(chunks_a.len(), chunks_b.len(), "chunk count mismatch");
        for (i, (a, b)) in chunks_a.iter().zip(chunks_b.iter()).enumerate() {
            prop_assert_eq!(
                &a.chunk_id, &b.chunk_id,
                "chunk_id not deterministic at index {}", i
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Additional invariants
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// 29. Every chunk has non-empty text.
    #[test]
    fn chunk_text_nonempty(
        inputs in arb_chunk_input_events_single_pane(1..=20),
    ) {
        let config = ChunkPolicyConfig::default();
        let chunks = build_semantic_chunks(&inputs, &config);
        for (i, chunk) in chunks.iter().enumerate() {
            prop_assert!(
                !chunk.text.is_empty(),
                "chunk text is empty at chunk {}", i
            );
        }
    }

    /// 30. Every chunk has event_count > 0.
    #[test]
    fn chunk_event_count_positive(
        inputs in arb_chunk_input_events_single_pane(1..=20),
    ) {
        let config = ChunkPolicyConfig::default();
        let chunks = build_semantic_chunks(&inputs, &config);
        for (i, chunk) in chunks.iter().enumerate() {
            prop_assert!(
                chunk.event_count > 0,
                "event_count is 0 at chunk {}", i
            );
        }
    }

    /// 31. Chunk IDs are unique within a single invocation.
    #[test]
    fn chunk_ids_unique(
        inputs in arb_chunk_input_events_single_pane(2..=20),
    ) {
        let config = ChunkPolicyConfig::default();
        let chunks = build_semantic_chunks(&inputs, &config);
        let ids: Vec<&str> = chunks.iter().map(|c| c.chunk_id.as_str()).collect();
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                prop_assert_ne!(
                    ids[i], ids[j],
                    "duplicate chunk_id at indices {} and {}", i, j
                );
            }
        }
    }

    /// 32. SemanticChunk serde JSON roundtrip preserves all fields.
    #[test]
    fn semantic_chunk_serde_roundtrip(
        text in arb_text(),
        pane_id in arb_pane_id(),
    ) {
        let event = make_egress_event("evt-serde", pane_id, &text, 2000, 0);
        let inputs = vec![make_chunk_input(event, make_offset(1, 0, 0))];
        let config = ChunkPolicyConfig::default();
        let chunks = build_semantic_chunks(&inputs, &config);
        prop_assert!(!chunks.is_empty(), "expected at least 1 chunk");

        for (i, chunk) in chunks.iter().enumerate() {
            let json = serde_json::to_string(chunk).expect("serialize chunk");
            let deserialized: SemanticChunk =
                serde_json::from_str(&json).expect("deserialize chunk");
            prop_assert_eq!(
                chunk, &deserialized,
                "serde roundtrip mismatch at chunk {}", i
            );
        }
    }

    /// 33. EgressOutput with is_gap=true is treated as boundary-only (no text contribution).
    #[test]
    fn egress_gap_is_boundary_only(
        text in arb_text(),
        pane_id in arb_pane_id(),
    ) {
        let event = RecorderEvent {
            schema_version: "ft.recorder.event.v1".to_string(),
            event_id: "evt-gap".to_string(),
            pane_id,
            session_id: Some("test-session".to_string()),
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms: 1000,
            recorded_at_ms: 1001,
            sequence: 0,
            causality: make_causality(),
            payload: RecorderEventPayload::EgressOutput {
                text,
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                segment_kind: RecorderSegmentKind::Gap,
                is_gap: true,
            },
        };

        let inputs = vec![make_chunk_input(event, make_offset(1, 0, 0))];
        let config = ChunkPolicyConfig::default();
        let chunks = build_semantic_chunks(&inputs, &config);
        prop_assert!(
            chunks.is_empty(),
            "is_gap=true should produce no chunks, got {}", chunks.len()
        );
    }

    /// 34. Multiple events on the same pane with close timestamps are grouped.
    #[test]
    fn close_timestamps_grouped(
        pane_id in arb_pane_id(),
        n in 2usize..=10,
    ) {
        let config = ChunkPolicyConfig::default();
        let inputs: Vec<ChunkInputEvent> = (0..n)
            .map(|i| {
                let seq = i as u64;
                let ts = 1000 + seq * 100; // 100ms apart -- well within hard_gap_ms
                let event = make_egress_event(
                    &format!("evt-{}", seq),
                    pane_id,
                    "short output text",
                    ts,
                    seq,
                );
                make_chunk_input(event, make_offset(1, seq, seq * 64))
            })
            .collect();

        let chunks = build_semantic_chunks(&inputs, &config);
        // With short texts and close timestamps, everything should merge into
        // a small number of chunks (likely 1).
        prop_assert!(
            !chunks.is_empty(),
            "expected at least 1 chunk"
        );
        // All events should be accounted for.
        let total_events: usize = chunks.iter().map(|c| c.event_count).sum();
        prop_assert!(
            total_events >= n,
            "total events {} < input count {}", total_events, n
        );
    }

    /// 35. Overlap from_chunk_id references a chunk that exists in the output (when consecutive).
    #[test]
    fn overlap_references_existing_chunk(
        inputs in arb_chunk_input_events_single_pane(3..=20),
    ) {
        let config = ChunkPolicyConfig::default();
        let chunks = build_semantic_chunks(&inputs, &config);
        let all_ids: std::collections::HashSet<&str> =
            chunks.iter().map(|c| c.chunk_id.as_str()).collect();

        for (i, chunk) in chunks.iter().enumerate() {
            if let Some(ref overlap) = chunk.overlap {
                prop_assert!(
                    all_ids.contains(overlap.from_chunk_id.as_str()),
                    "overlap.from_chunk_id '{}' at chunk {} not found in output chunk IDs",
                    overlap.from_chunk_id, i
                );
            }
        }
    }
}
