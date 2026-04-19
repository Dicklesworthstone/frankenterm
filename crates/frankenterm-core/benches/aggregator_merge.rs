//! Criterion benchmarks for Aggregator ingest throughput and scaling.
//!
//! Bead: wa-nu4.4.3.3
//! Required coverage:
//! - single-agent ingest events/sec (varying payload sizes: 256B, 4KB, 64KB)
//! - multi-agent scaling (1, 5, 20 concurrent senders)
//! - duplicate rejection throughput
//! - stale agent pruning performance
//! - envelope JSON serde overhead

use std::hint::black_box;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::wire_protocol::{
    Aggregator, IngestResult, PaneDelta, PaneMeta, WireEnvelope, WirePayload,
};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "aggregator_merge/single_agent_ingest/256B",
        budget: "single-agent 256B payload ingest >10K events/sec",
    },
    bench_common::BenchBudget {
        name: "aggregator_merge/single_agent_ingest/4KB",
        budget: "single-agent 4KB payload ingest stays bounded",
    },
    bench_common::BenchBudget {
        name: "aggregator_merge/single_agent_ingest/64KB",
        budget: "single-agent 64KB payload ingest stays bounded",
    },
    bench_common::BenchBudget {
        name: "aggregator_merge/multi_agent_scaling",
        budget: "multi-agent (1/5/20) ingest scales sub-linearly",
    },
    bench_common::BenchBudget {
        name: "aggregator_merge/dedup_rejection",
        budget: "duplicate rejection is cheaper than fresh ingest",
    },
    bench_common::BenchBudget {
        name: "aggregator_merge/stale_pruning",
        budget: "prune_stale_agents is bounded for 100 agents",
    },
    bench_common::BenchBudget {
        name: "aggregator_merge/envelope_serde",
        budget: "envelope JSON roundtrip stays low-overhead",
    },
];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_pane_delta_payload(content_size: usize) -> WirePayload {
    let content: String = "x".repeat(content_size);
    WirePayload::PaneDelta(PaneDelta {
        pane_id: 1,
        seq: 0,
        content_len: content.len(),
        content,
        captured_at_ms: 1_700_000_000_000,
    })
}

fn make_pane_meta_payload() -> WirePayload {
    WirePayload::PaneMeta(PaneMeta {
        pane_id: 1,
        pane_uuid: Some("bench-uuid-0001".into()),
        domain: "local".into(),
        title: Some("bench shell".into()),
        cwd: Some("/tmp".into()),
        rows: Some(24),
        cols: Some(80),
        observed: true,
        timestamp_ms: 1_700_000_000_000,
    })
}

fn make_envelope(sender: &str, seq: u64, payload: WirePayload) -> WireEnvelope {
    WireEnvelope {
        version: frankenterm_core::wire_protocol::PROTOCOL_VERSION,
        seq,
        sender: sender.into(),
        sent_at_ms: 1_700_000_000_000,
        payload,
    }
}

/// Pre-serialize an envelope so we can bench `ingest(bytes)`.
fn envelope_bytes(sender: &str, seq: u64, payload: WirePayload) -> Vec<u8> {
    let envelope = make_envelope(sender, seq, payload);
    envelope.to_json().expect("serialize envelope")
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

fn bench_single_agent_ingest(c: &mut Criterion) {
    let mut group = c.benchmark_group("aggregator_merge/single_agent_ingest");
    group.measurement_time(Duration::from_secs(8));

    for &(label, content_size) in &[("256B", 256), ("4KB", 4096), ("64KB", 65536)] {
        // Pre-generate payloads for sequential seq values.
        // We'll generate enough envelopes for the measurement window.
        let batch_size: u64 = 1000;
        let envelopes: Vec<Vec<u8>> = (0..batch_size)
            .map(|seq| envelope_bytes("agent-bench", seq, make_pane_delta_payload(content_size)))
            .collect();

        group.throughput(Throughput::Elements(batch_size));
        group.bench_with_input(BenchmarkId::new("raw_json", label), &envelopes, |b, envelopes| {
            b.iter(|| {
                let mut agg = Aggregator::new(16);
                for bytes in envelopes {
                    let result = agg.ingest(black_box(bytes));
                    black_box(&result);
                }
                black_box(agg.total_accepted());
            });
        });

        // Also bench the pre-decoded path (ingest_envelope_at).
        let decoded: Vec<WireEnvelope> = (0..batch_size)
            .map(|seq| make_envelope("agent-bench", seq, make_pane_delta_payload(content_size)))
            .collect();

        group.bench_with_input(
            BenchmarkId::new("pre_decoded", label),
            &decoded,
            |b, decoded| {
                b.iter(|| {
                    let mut agg = Aggregator::new(16);
                    let now = 1_700_000_000_000i64;
                    for envelope in decoded {
                        let result = agg.ingest_envelope_at(black_box(envelope.clone()), now);
                        black_box(&result);
                    }
                    black_box(agg.total_accepted());
                });
            },
        );
    }

    group.finish();
}

fn bench_multi_agent_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("aggregator_merge/multi_agent_scaling");
    group.measurement_time(Duration::from_secs(8));

    for &agent_count in &[1usize, 5, 20] {
        let messages_per_agent = 200u64;
        let total = agent_count as u64 * messages_per_agent;

        // Pre-build all envelopes: round-robin across agents.
        let envelopes: Vec<WireEnvelope> = (0..agent_count)
            .flat_map(|a| {
                let sender = format!("agent-{a}");
                (0..messages_per_agent)
                    .map(move |seq| make_envelope(&sender, seq, make_pane_meta_payload()))
            })
            .collect();

        group.throughput(Throughput::Elements(total));
        group.bench_with_input(
            BenchmarkId::new("agents", agent_count),
            &envelopes,
            |b, envelopes| {
                b.iter(|| {
                    let mut agg = Aggregator::new(64);
                    let now = 1_700_000_000_000i64;
                    for envelope in envelopes {
                        let result = agg.ingest_envelope_at(black_box(envelope.clone()), now);
                        black_box(&result);
                    }
                    black_box(agg.total_accepted());
                });
            },
        );
    }

    group.finish();
}

fn bench_dedup_rejection(c: &mut Criterion) {
    let mut group = c.benchmark_group("aggregator_merge/dedup_rejection");
    group.measurement_time(Duration::from_secs(5));

    let batch_size = 1000u64;

    // Pre-build envelopes: first `batch_size` are fresh, then repeat same seqs.
    let fresh: Vec<WireEnvelope> = (0..batch_size)
        .map(|seq| make_envelope("agent-dedup", seq, make_pane_meta_payload()))
        .collect();

    let duplicates: Vec<WireEnvelope> = (0..batch_size)
        .map(|seq| make_envelope("agent-dedup", seq, make_pane_meta_payload()))
        .collect();

    group.throughput(Throughput::Elements(batch_size));

    // Baseline: fresh ingest
    group.bench_function("fresh_ingest", |b| {
        b.iter(|| {
            let mut agg = Aggregator::new(16);
            let now = 1_700_000_000_000i64;
            for envelope in &fresh {
                let result = agg.ingest_envelope_at(black_box(envelope.clone()), now);
                black_box(&result);
            }
            black_box(agg.total_accepted());
        });
    });

    // Dedup path: pre-seed then ingest duplicates
    group.bench_function("duplicate_rejection", |b| {
        b.iter(|| {
            let mut agg = Aggregator::new(16);
            let now = 1_700_000_000_000i64;
            // Seed all fresh envelopes first.
            for envelope in &fresh {
                let _ = agg.ingest_envelope_at(envelope.clone(), now);
            }
            // Now re-ingest the same seqs — all should be Duplicate.
            for envelope in &duplicates {
                let result = agg.ingest_envelope_at(black_box(envelope.clone()), now);
                debug_assert!(matches!(result, Ok(IngestResult::Duplicate { .. })));
                black_box(&result);
            }
            black_box(agg.total_rejected());
        });
    });

    group.finish();
}

fn bench_stale_pruning(c: &mut Criterion) {
    let mut group = c.benchmark_group("aggregator_merge/stale_pruning");

    for &agent_count in &[10usize, 50, 100] {
        group.bench_with_input(
            BenchmarkId::new("agents", agent_count),
            &agent_count,
            |b, &agent_count| {
                b.iter(|| {
                    let stale_after_ms = 5_000i64;
                    let mut agg = Aggregator::with_stale_after(256, stale_after_ms);
                    let base_ts = 1_700_000_000_000i64;

                    // Insert agents at staggered times: half are stale.
                    for i in 0..agent_count {
                        let sender = format!("agent-{i}");
                        let ts = if i % 2 == 0 {
                            base_ts // old
                        } else {
                            base_ts + stale_after_ms + 1000 // recent
                        };
                        let envelope = make_envelope(&sender, 0, make_pane_meta_payload());
                        let _ = agg.ingest_envelope_at(envelope, ts);
                    }

                    let pruned =
                        agg.prune_stale_agents(black_box(base_ts + stale_after_ms + 2000));
                    black_box(pruned);
                });
            },
        );
    }

    group.finish();
}

fn bench_envelope_serde(c: &mut Criterion) {
    let mut group = c.benchmark_group("aggregator_merge/envelope_serde");

    let small_envelope = make_envelope("agent-serde", 42, make_pane_meta_payload());
    let small_json = small_envelope.to_json().expect("serialize");

    let large_envelope = make_envelope("agent-serde", 42, make_pane_delta_payload(4096));
    let large_json = large_envelope.to_json().expect("serialize");

    group.bench_function("serialize_small", |b| {
        b.iter(|| {
            let json = black_box(&small_envelope).to_json().expect("serialize");
            black_box(json.len());
        });
    });

    group.bench_function("deserialize_small", |b| {
        b.iter(|| {
            let envelope = WireEnvelope::from_json(black_box(&small_json)).expect("deserialize");
            black_box(&envelope);
        });
    });

    group.bench_function("serialize_4kb", |b| {
        b.iter(|| {
            let json = black_box(&large_envelope).to_json().expect("serialize");
            black_box(json.len());
        });
    });

    group.bench_function("deserialize_4kb", |b| {
        b.iter(|| {
            let envelope = WireEnvelope::from_json(black_box(&large_json)).expect("deserialize");
            black_box(&envelope);
        });
    });

    group.bench_function("roundtrip_small", |b| {
        b.iter(|| {
            let json = black_box(&small_envelope).to_json().expect("serialize");
            let decoded = WireEnvelope::from_json(black_box(&json)).expect("deserialize");
            black_box(&decoded);
        });
    });

    group.finish();
}

fn bench_suite(c: &mut Criterion) {
    bench_single_agent_ingest(c);
    bench_multi_agent_scaling(c);
    bench_dedup_rejection(c);
    bench_stale_pruning(c);
    bench_envelope_serde(c);
    bench_common::emit_bench_artifacts("aggregator_merge", BUDGETS);
}

criterion_group!(benches, bench_suite);
criterion_main!(benches);
