//! Criterion coverage for TOON encoder hot paths.
//!
//! The payloads mirror robot/MCP response families that use
//! `toon_rust::encode(value, None)` when callers request TOON output.

use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use serde_json::{Value, json};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "toon_encoding_wa_state",
        budget: "compare JSON and TOON encode cost across 1/10/50/200 pane envelopes",
    },
    bench_common::BenchBudget {
        name: "toon_encoding_wa_search",
        budget: "compare JSON and TOON encode cost across 10/100/500/1000 search hits",
    },
    bench_common::BenchBudget {
        name: "toon_encoding_wa_events",
        budget: "compare JSON and TOON encode cost across 10/100/500/1000 event envelopes",
    },
];

fn pane(index: usize) -> Value {
    json!({
        "pane_id": index as u64,
        "pane_uuid": format!("pane-uuid-{index:04}"),
        "tab_id": index / 4,
        "window_id": index / 16,
        "domain": if index % 7 == 0 { "ssh:builder" } else { "local" },
        "title": format!("cod_{index}_worker"),
        "cwd": format!("/Users/jemanuel/projects/frankenterm/session-{index:04}"),
        "observed": index % 11 != 0,
        "ignore_reason": if index % 11 == 0 { Some("domain filtered") } else { None },
        "state": {
            "agent": if index % 3 == 0 { "codex" } else { "claude_code" },
            "status": if index % 5 == 0 { "busy" } else { "idle" },
            "last_seen_ms": 1_777_200_000_000u64 + index as u64,
            "tail_bytes": 8192 + (index * 17)
        }
    })
}

fn wa_state_payload(panes: usize) -> Value {
    json!({
        "ok": true,
        "data": {
            "panes": (0..panes).map(pane).collect::<Vec<_>>(),
            "tail_lines": 200,
            "escapes_included": false
        },
        "elapsed_ms": 7,
        "version": "bench",
        "now": 1_777_200_000_000u64,
        "mcp_version": "2025-03-26"
    })
}

fn search_hit(index: usize) -> Value {
    json!({
        "pane_id": (index % 200) as u64,
        "score": 1.0 / ((index + 1) as f64),
        "line_number": index * 3 + 10,
        "text": format!("search result {index}: build output mentions TOON payload serialization and pane state"),
        "snippet": format!("... result {index} with <mark>TOON</mark> and surrounding context ..."),
        "timestamp_ms": 1_777_200_000_000u64 + index as u64,
        "metadata": {
            "source": "capture",
            "session_id": format!("sess-{index:04}"),
            "rank": index
        }
    })
}

fn wa_search_payload(hits: usize) -> Value {
    json!({
        "ok": true,
        "data": {
            "query": "TOON payload serialization",
            "hits": (0..hits).map(search_hit).collect::<Vec<_>>(),
            "total": hits,
            "limit": hits,
            "backend": "hybrid"
        },
        "elapsed_ms": 12,
        "version": "bench",
        "now": 1_777_200_000_123u64,
        "mcp_version": "2025-03-26"
    })
}

fn event(index: usize) -> Value {
    json!({
        "id": format!("evt-{index:06}"),
        "pane_id": (index % 200) as u64,
        "rule_id": if index % 4 == 0 { "codex.done" } else { "codex.progress" },
        "severity": if index % 13 == 0 { "warning" } else { "info" },
        "timestamp_ms": 1_777_200_001_000u64 + index as u64,
        "handled": index % 2 == 0,
        "payload": {
            "message": format!("event {index} observed while serializing robot output"),
            "matched_text": "Done",
            "confidence": 0.91,
            "labels": ["robot", "mcp", "toon"]
        }
    })
}

fn wa_events_payload(events: usize) -> Value {
    json!({
        "ok": true,
        "data": {
            "events": (0..events).map(event).collect::<Vec<_>>(),
            "total": events,
            "unhandled": events / 2
        },
        "elapsed_ms": 9,
        "version": "bench",
        "now": 1_777_200_000_456u64,
        "mcp_version": "2025-03-26"
    })
}

fn json_bytes(value: &Value) -> usize {
    serde_json::to_vec(value)
        .expect("benchmark fixture serializes")
        .len()
}

fn toon_bytes(value: &Value) -> usize {
    toon_rust::encode(value.clone(), None).len()
}

fn bench_payload(
    c: &mut Criterion,
    group_name: &str,
    scales: &[usize],
    fixture: fn(usize) -> Value,
) {
    let mut group = c.benchmark_group(group_name);
    group.sample_size(20);

    for &scale in scales {
        let value = fixture(scale);
        let json_len = json_bytes(&value);
        let toon_len = toon_bytes(&value);
        let toon_text = toon_rust::encode(value.clone(), None);

        group.throughput(Throughput::Bytes(json_len as u64));
        group.bench_with_input(
            BenchmarkId::new("json_encode", scale),
            &value,
            |b, value| {
                b.iter(|| serde_json::to_string(black_box(value)).expect("serialize json"));
            },
        );

        group.throughput(Throughput::Bytes(toon_len as u64));
        group.bench_with_input(
            BenchmarkId::new("toon_encode", scale),
            &value,
            |b, value| {
                b.iter_batched(
                    || value.clone(),
                    |value| toon_rust::encode(black_box(value), None),
                    BatchSize::SmallInput,
                );
            },
        );

        group.throughput(Throughput::Bytes(toon_len as u64));
        group.bench_with_input(
            BenchmarkId::new("toon_decode", scale),
            &toon_text,
            |b, toon_text| {
                b.iter(|| toon_rust::try_decode(black_box(toon_text), None).expect("decode TOON"));
            },
        );
    }

    group.finish();
}

fn bench_toon_encoding(c: &mut Criterion) {
    bench_common::emit_bench_artifacts("toon_encoding", BUDGETS);

    bench_payload(
        c,
        "toon_encoding_wa_state",
        &[1, 10, 50, 200],
        wa_state_payload,
    );
    bench_payload(
        c,
        "toon_encoding_wa_search",
        &[10, 100, 500, 1000],
        wa_search_payload,
    );
    bench_payload(
        c,
        "toon_encoding_wa_events",
        &[10, 100, 500, 1000],
        wa_events_payload,
    );
}

criterion_group!(benches, bench_toon_encoding);
criterion_main!(benches);
