//! Semantic chunking ingest hot path (ft-o2mtn).
//!
//! ## Why this exists
//!
//! `build_semantic_chunks` is the deterministic windowing policy
//! every search-index ingest call walks (recorder events →
//! `SemanticChunk` vector → embedding/indexing pipelines). It runs
//! on every `ft.recorder.chunking.v1` ingest tick — and yet had
//! zero criterion coverage at branch HEAD. ft-3r0n4's wa.state
//! fleet bench measures envelope construction; this measures the
//! sibling chunking step that feeds the search index.
//!
//! ## Workloads
//!
//! Three pipelines × three line counts (100, 1000, 10000):
//!
//! | Pipeline               | Steps included                     |
//! |------------------------|------------------------------------|
//! | construct_only         | Build `Vec<ChunkInputEvent>` only  |
//! | chunk_default          | Construct + build_semantic_chunks (default config) |
//! | chunk_smaller_chunks   | Construct + build with max_chunk_chars=512 |
//!
//! Subtraction of group medians attributes:
//!   chunking_default = chunk_default - construct_only
//!   chunking_smaller = chunk_smaller_chunks - construct_only
//!   smaller_overhead = chunking_smaller - chunking_default
//!
//! ## Adversarial fields
//!
//! Each synthetic event mixes ingress + egress (forces glue-rule
//! evaluation), interleaves a control marker every 17 events
//! (forces hard-boundary flush), and varies text length 8..256
//! chars so soft-split + overlap evaluation actually fires (not a
//! single-monster-chunk benchmark).
//!
//! ## Output
//!
//! Hypothesis vs measured documented at
//! `docs/perf-ledger/build-semantic-chunks.md`.

use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::recorder_storage::RecorderOffset;
use frankenterm_core::recording::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderControlMarkerType, RecorderEvent,
    RecorderEventCausality, RecorderEventPayload, RecorderEventSource, RecorderIngressKind,
    RecorderRedactionLevel, RecorderSegmentKind, RecorderTextEncoding,
};
use frankenterm_core::search::{ChunkInputEvent, ChunkPolicyConfig, build_semantic_chunks};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "semantic_chunks_construct_only",
        budget: "build Vec<ChunkInputEvent> for 100/1000/10000 events (no chunking)",
    },
    bench_common::BenchBudget {
        name: "semantic_chunks_default",
        budget: "build_semantic_chunks with default policy at 100/1000/10000 events",
    },
    bench_common::BenchBudget {
        name: "semantic_chunks_smaller",
        budget: "build_semantic_chunks with max_chunk_chars=512 at 100/1000/10000 events",
    },
];

const EVENT_COUNTS: &[usize] = &[100, 1_000, 10_000];

fn make_causality() -> RecorderEventCausality {
    RecorderEventCausality {
        parent_event_id: None,
        trigger_event_id: None,
        root_event_id: None,
    }
}

/// Synthetic recorder event mirroring the shape ingested by the
/// chunking pipeline. Mixes ingress + egress + control markers and
/// varies text length to force glue/soft-split/overlap branches —
/// not a degenerate single-chunk no-op measurement.
fn synth_event(index: usize) -> RecorderEvent {
    let pane_id = (index % 4) as u64;
    let occurred_at_ms = 1_777_200_000_000u64 + (index as u64 * 50);
    let event_id = format!("evt-{index:08}");
    let session_id = Some(format!("sess-{}", index / 256));

    // Every 17th event is a control marker (forces hard boundary).
    if index % 17 == 0 {
        return RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id,
            pane_id,
            session_id,
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms,
            recorded_at_ms: occurred_at_ms + 1,
            sequence: index as u64,
            causality: make_causality(),
            payload: RecorderEventPayload::ControlMarker {
                control_marker_type: RecorderControlMarkerType::PromptBoundary,
                details: serde_json::Value::Null,
            },
        };
    }

    // Vary text length 8..256 chars so soft-split + overlap evaluate.
    let text_len = 8 + (index * 31) % 248;
    let text: String = (0..text_len)
        .map(|byte_index| {
            let base = b'a' + ((index + byte_index) % 26) as u8;
            base as char
        })
        .collect();

    // Half ingress, half egress — exercises direction-change boundaries.
    if index % 2 == 0 {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id,
            pane_id,
            session_id,
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::RobotMode,
            occurred_at_ms,
            recorded_at_ms: occurred_at_ms + 1,
            sequence: index as u64,
            causality: make_causality(),
            payload: RecorderEventPayload::IngressText {
                text,
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        }
    } else {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id,
            pane_id,
            session_id,
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms,
            recorded_at_ms: occurred_at_ms + 1,
            sequence: index as u64,
            causality: make_causality(),
            payload: RecorderEventPayload::EgressOutput {
                text,
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                segment_kind: RecorderSegmentKind::Delta,
                is_gap: false,
            },
        }
    }
}

fn synth_offset(index: usize) -> RecorderOffset {
    RecorderOffset {
        segment_id: (index / 1024) as u64,
        ordinal: index as u64,
        byte_offset: (index as u64) * 64,
    }
}

fn construct_events(count: usize) -> Vec<ChunkInputEvent> {
    (0..count)
        .map(|index| ChunkInputEvent {
            event: synth_event(index),
            offset: synth_offset(index),
        })
        .collect()
}

// ── construct_only ─────────────────────────────────────────────

fn bench_construct_only(c: &mut Criterion) {
    let mut group = c.benchmark_group("semantic_chunks_construct_only");
    group.sample_size(40);
    for &count in EVENT_COUNTS {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter(|| {
                let events = construct_events(black_box(count));
                black_box(events);
            });
        });
    }
    group.finish();
}

// ── chunking with default policy ───────────────────────────────

fn bench_chunk_default(c: &mut Criterion) {
    let mut group = c.benchmark_group("semantic_chunks_default");
    group.sample_size(30);
    let config = ChunkPolicyConfig::default();
    for &count in EVENT_COUNTS {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter_batched(
                || construct_events(count),
                |events| {
                    let chunks = build_semantic_chunks(black_box(&events), &config);
                    black_box(chunks);
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

// ── chunking with smaller chunk size ───────────────────────────

fn bench_chunk_smaller(c: &mut Criterion) {
    let mut group = c.benchmark_group("semantic_chunks_smaller");
    group.sample_size(30);
    // Smaller chunks → more chunks → more soft-split + overlap copies.
    let config = ChunkPolicyConfig {
        max_chunk_chars: 512,
        max_chunk_events: 16,
        overlap_chars: 64,
        ..Default::default()
    };
    for &count in EVENT_COUNTS {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter_batched(
                || construct_events(count),
                |events| {
                    let chunks = build_semantic_chunks(black_box(&events), &config);
                    black_box(chunks);
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_semantic_chunks(c: &mut Criterion) {
    bench_common::emit_bench_artifacts("semantic_chunks", BUDGETS);
    bench_construct_only(c);
    bench_chunk_default(c);
    bench_chunk_smaller(c);
}

criterion_group!(benches, bench_semantic_chunks);
criterion_main!(benches);
