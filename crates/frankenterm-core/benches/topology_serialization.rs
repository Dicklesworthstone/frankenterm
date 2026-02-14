//! Benchmarks for session topology serialization.
//!
//! Performance budgets:
//! - from_panes (10 panes): **< 100µs**
//! - from_panes (50 panes): **< 500µs**
//! - JSON roundtrip: **< 50µs** per snapshot
//! - Pane matching: **< 200µs** for 50-pane topology

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::session_topology::TopologySnapshot;
use frankenterm_core::wezterm::{PaneInfo, PaneSize};
use std::collections::HashMap;
#[cfg(feature = "mcp-server")]
use std::hint::black_box;

#[cfg(feature = "mcp-server")]
use serde_json::{Value, json};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "from_panes_small",
        budget: "p50 < 100us (10-pane topology)",
    },
    bench_common::BenchBudget {
        name: "from_panes_medium",
        budget: "p50 < 500us (50-pane topology)",
    },
    bench_common::BenchBudget {
        name: "json_roundtrip",
        budget: "p50 < 50us per snapshot",
    },
    bench_common::BenchBudget {
        name: "pane_matching",
        budget: "p50 < 200us (50-pane matching)",
    },
    bench_common::BenchBudget {
        name: "toon_vs_json_encode",
        budget: "p50 < 50ms for representative MCP envelopes",
    },
    bench_common::BenchBudget {
        name: "toon_vs_json_decode",
        budget: "p50 < 50ms for representative MCP envelopes",
    },
    bench_common::BenchBudget {
        name: "toon_stream_decode_10mb",
        budget: "p50 < 200ms for ~10MB TOON line stream",
    },
];

/// Generate mock PaneInfo entries simulating a multi-window layout.
fn generate_panes(count: usize) -> Vec<PaneInfo> {
    let mut panes = Vec::with_capacity(count);
    let panes_per_tab = 4;
    let tabs_per_window = 3;

    for i in 0..count {
        let window_id = (i / (panes_per_tab * tabs_per_window)) as u64;
        let tab_id = (i / panes_per_tab) as u64;

        panes.push(PaneInfo {
            window_id,
            tab_id,
            pane_id: i as u64,
            domain_id: None,
            domain_name: None,
            workspace: Some("default".to_string()),
            size: Some(PaneSize {
                rows: 24,
                cols: 80,
                pixel_width: Some(640),
                pixel_height: Some(384),
                dpi: None,
            }),
            rows: None,
            cols: None,
            title: Some(format!("pane-{i}")),
            cwd: Some(format!("file:///home/user/project-{i}")),
            tty_name: None,
            cursor_x: Some(0),
            cursor_y: Some(0),
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: i == 0,
            is_zoomed: false,
            extra: HashMap::new(),
        });
    }
    panes
}

/// Generate a TopologySnapshot with a known structure for matching benchmarks.
fn generate_topology(pane_count: usize) -> TopologySnapshot {
    let panes = generate_panes(pane_count);
    let now_ms = 1700000000000u64;
    let (snapshot, _report) = TopologySnapshot::from_panes(&panes, now_ms);
    snapshot
}

#[cfg(feature = "mcp-server")]
#[derive(Clone)]
struct SerializationPayload {
    name: &'static str,
    value: Value,
    json: String,
    toon: String,
}

#[cfg(feature = "mcp-server")]
fn estimate_tokens(s: &str) -> usize {
    let chars = s.len();
    let words = s.split_whitespace().count();
    std::cmp::max(chars / 4, words)
}

#[cfg(feature = "mcp-server")]
fn make_payload(name: &'static str, value: Value) -> SerializationPayload {
    let json = serde_json::to_string(&value).expect("serialize benchmark payload to json");
    let toon = toon_rust::encode(value.clone(), None);
    SerializationPayload {
        name,
        value,
        json,
        toon,
    }
}

#[cfg(feature = "mcp-server")]
fn representative_payloads() -> Vec<SerializationPayload> {
    let state_payload = make_payload(
        "state_12_panes",
        json!({
            "ok": true,
            "data": {
                "panes": (0..12)
                    .map(|pane_id| json!({
                        "pane_id": pane_id,
                        "domain": "local",
                        "title": format!("agent-{pane_id}"),
                        "cwd": format!("/workspace/project-{pane_id}")
                    }))
                    .collect::<Vec<_>>()
            },
            "elapsed_ms": 3,
            "version": "0.1.0",
            "now": 1_700_000_000_000_u64,
            "mcp_version": "v1"
        }),
    );

    let search_payload = make_payload(
        "search_100_hits",
        json!({
            "ok": true,
            "data": {
                "query": "error OR warning",
                "results": (0..100)
                    .map(|i| json!({
                        "segment_id": i + 10_000,
                        "pane_id": i % 16,
                        "seq": i * 5,
                        "captured_at": 1_700_000_000_000_i64 + i,
                        "score": 0.99_f64 - (i as f64 * 0.001),
                        "snippet": format!("build-{i}: warning: retry budget nearly exhausted"),
                    }))
                    .collect::<Vec<_>>()
            },
            "elapsed_ms": 12,
            "version": "0.1.0",
            "now": 1_700_000_000_000_u64,
            "mcp_version": "v1"
        }),
    );

    let pane_text = (0..8_000)
        .map(|i| format!("line-{i:05}: compilation unit {i} completed"))
        .collect::<Vec<_>>()
        .join("\n");
    let get_text_payload = make_payload(
        "get_text_large",
        json!({
            "ok": true,
            "data": {
                "pane_id": 7,
                "text": pane_text,
                "tail_lines": 8_000,
                "escapes_included": false,
                "truncated": false
            },
            "elapsed_ms": 21,
            "version": "0.1.0",
            "now": 1_700_000_000_000_u64,
            "mcp_version": "v1"
        }),
    );

    vec![state_payload, search_payload, get_text_payload]
}

#[cfg(feature = "mcp-server")]
fn large_stream_input() -> (Vec<String>, usize) {
    let message = "x".repeat(256);
    let records = (0..32_000)
        .map(|i| {
            json!({
                "seq": i,
                "pane_id": i % 32,
                "rule_id": "core.codex:usage_reached",
                "event_type": "usage_reached",
                "message": message.clone()
            })
        })
        .collect::<Vec<_>>();
    let value = json!({
        "ok": true,
        "data": {
            "events": records
        },
        "elapsed_ms": 150,
        "version": "0.1.0",
        "now": 1_700_000_000_000_u64,
        "mcp_version": "v1"
    });
    let toon = toon_rust::encode(value, None);
    let size_bytes = toon.len();
    let lines = toon
        .lines()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>();
    (lines, size_bytes)
}

fn bench_from_panes(c: &mut Criterion) {
    let mut group = c.benchmark_group("topology/from_panes");

    for &count in &[4, 10, 20, 50] {
        let panes = generate_panes(count);
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &panes, |b, panes| {
            b.iter(|| {
                let now_ms = 1700000000000u64;
                TopologySnapshot::from_panes(panes, now_ms)
            });
        });
    }

    group.finish();
}

fn bench_json_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("topology/json_roundtrip");

    for &count in &[4, 10, 50] {
        let snapshot = generate_topology(count);
        let json = snapshot.to_json().unwrap();

        group.throughput(Throughput::Bytes(json.len() as u64));

        group.bench_with_input(
            BenchmarkId::new("serialize", count),
            &snapshot,
            |b, snap| {
                b.iter(|| snap.to_json().unwrap());
            },
        );

        group.bench_with_input(BenchmarkId::new("deserialize", count), &json, |b, json| {
            b.iter(|| TopologySnapshot::from_json(json).unwrap());
        });
    }

    group.finish();
}

fn bench_pane_count(c: &mut Criterion) {
    let mut group = c.benchmark_group("topology/pane_count");

    for &count in &[10, 50, 100] {
        let snapshot = generate_topology(count);
        group.bench_with_input(BenchmarkId::from_parameter(count), &snapshot, |b, snap| {
            b.iter(|| snap.pane_count());
        });
    }

    group.finish();
}

fn bench_pane_ids(c: &mut Criterion) {
    let mut group = c.benchmark_group("topology/pane_ids");

    for &count in &[10, 50, 100] {
        let snapshot = generate_topology(count);
        group.bench_with_input(BenchmarkId::from_parameter(count), &snapshot, |b, snap| {
            b.iter(|| snap.pane_ids());
        });
    }

    group.finish();
}

fn bench_pane_matching(c: &mut Criterion) {
    let mut group = c.benchmark_group("topology/pane_matching");

    for &count in &[4, 10, 50] {
        let old_snapshot = generate_topology(count);

        // New panes: similar but with shuffled IDs (simulating restore)
        let new_panes: Vec<PaneInfo> = generate_panes(count)
            .into_iter()
            .map(|mut p| {
                p.pane_id += 100; // different IDs
                p
            })
            .collect();

        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &(&old_snapshot, &new_panes),
            |b, (old, new)| {
                b.iter(|| frankenterm_core::session_topology::match_panes(old, new));
            },
        );
    }

    group.finish();
}

#[cfg(feature = "mcp-server")]
fn bench_toon_vs_json_serialization(c: &mut Criterion) {
    let payloads = representative_payloads();

    let mut encode_group = c.benchmark_group("toon_serialization/encode");
    for payload in &payloads {
        encode_group.throughput(Throughput::Bytes(payload.json.len() as u64));
        encode_group.bench_with_input(
            BenchmarkId::new("json", payload.name),
            payload,
            |b, payload| {
                b.iter(|| {
                    let value = payload.value.clone();
                    black_box(serde_json::to_string(&value).expect("json encode"));
                });
            },
        );
        encode_group.bench_with_input(
            BenchmarkId::new("toon", payload.name),
            payload,
            |b, payload| {
                b.iter(|| {
                    let value = payload.value.clone();
                    black_box(toon_rust::encode(value, None));
                });
            },
        );
    }
    encode_group.finish();

    let mut decode_group = c.benchmark_group("toon_serialization/decode");
    for payload in &payloads {
        decode_group.throughput(Throughput::Bytes(payload.json.len() as u64));
        decode_group.bench_with_input(
            BenchmarkId::new("json", payload.name),
            payload,
            |b, payload| {
                b.iter(|| {
                    black_box(
                        serde_json::from_str::<serde_json::Value>(&payload.json)
                            .expect("json decode"),
                    );
                });
            },
        );
        decode_group.bench_with_input(
            BenchmarkId::new("toon", payload.name),
            payload,
            |b, payload| {
                b.iter(|| {
                    black_box(toon_rust::try_decode(&payload.toon, None).expect("toon decode"));
                });
            },
        );
    }
    decode_group.finish();

    let mut token_group = c.benchmark_group("toon_serialization/token_estimate");
    for payload in &payloads {
        token_group.bench_with_input(
            BenchmarkId::new("compare", payload.name),
            payload,
            |b, p| {
                b.iter(|| {
                    let json_tokens = estimate_tokens(&p.json);
                    let toon_tokens = estimate_tokens(&p.toon);
                    let savings_pct = if json_tokens > 0 {
                        100_i64 - ((toon_tokens as i64) * 100 / (json_tokens as i64))
                    } else {
                        0
                    };
                    black_box((json_tokens, toon_tokens, savings_pct));
                });
            },
        );
    }
    token_group.finish();
}

#[cfg(not(feature = "mcp-server"))]
fn bench_toon_vs_json_serialization(_c: &mut Criterion) {}

#[cfg(feature = "mcp-server")]
fn bench_toon_stream_decode(c: &mut Criterion) {
    let (lines, size_bytes) = large_stream_input();
    let mut group = c.benchmark_group("toon_serialization/stream_decode");
    group.throughput(Throughput::Bytes(size_bytes as u64));
    group.bench_function("toon_stream_sync_approx_10mb", |b| {
        b.iter(|| {
            let events = toon_rust::try_decode_stream_sync(lines.iter().cloned(), None)
                .expect("toon stream decode");
            black_box(events.len());
        });
    });
    group.finish();
}

#[cfg(not(feature = "mcp-server"))]
fn bench_toon_stream_decode(_c: &mut Criterion) {}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("topology_serialization", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_from_panes,
        bench_json_roundtrip,
        bench_pane_count,
        bench_pane_ids,
        bench_pane_matching,
        bench_toon_vs_json_serialization,
        bench_toon_stream_decode
);
criterion_main!(benches);
