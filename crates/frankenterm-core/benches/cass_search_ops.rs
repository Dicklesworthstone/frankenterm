//! Criterion benchmarks for cass (session search) integration hot-path operations.
//!
//! Bead: wa-2l9kn
//! Required coverage:
//! - CassSearchResult JSON deserialization for varying hit counts (10, 50, 200)
//! - parse_cass_timestamp_ms throughput for epoch-ms, epoch-s, and RFC3339
//! - CassSearchHit hint formatting throughput
//! - CassSession summary computation
//! - Serde roundtrip overhead for search result payloads

use std::collections::HashMap;
use std::hint::black_box;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::cass::{
    CassAgent, CassMessage, CassSearchHit, CassSearchResult, CassSession, CassSessionSummary,
    CassStatus, parse_cass_timestamp_ms,
};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "cass_search_ops/search_result_deser",
        budget: "CassSearchResult deser <500µs for 200-hit payload",
    },
    bench_common::BenchBudget {
        name: "cass_search_ops/timestamp_parse",
        budget: "parse_cass_timestamp_ms <100ns per call",
    },
    bench_common::BenchBudget {
        name: "cass_search_ops/hint_format",
        budget: "hint formatting <10µs per hit",
    },
    bench_common::BenchBudget {
        name: "cass_search_ops/session_summary",
        budget: "session summary compute <50µs for 100-message session",
    },
    bench_common::BenchBudget {
        name: "cass_search_ops/serde_roundtrip",
        budget: "search result serde roundtrip <1ms for 200 hits",
    },
];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_search_hit(i: usize) -> CassSearchHit {
    CassSearchHit {
        source_path: Some(format!("/home/user/.local/share/cass/sessions/session-{i:04}.jsonl")),
        line_number: Some(i * 10 + 5),
        agent: Some(if i % 3 == 0 { "codex" } else { "claude_code" }.to_string()),
        workspace: Some(format!("/workspace/project-{}", i % 8)),
        content: Some(format!(
            "error[E{:04}]: mismatched types at line {}: expected `usize`, found `i64`",
            300 + (i % 50),
            i * 10 + 5
        )),
        timestamp: Some(format!("2026-04-{:02}T12:00:00Z", (i % 28) + 1)),
        score: Some(0.95 - (i as f64 * 0.003)),
        extra: HashMap::new(),
    }
}

fn make_search_result(hit_count: usize) -> CassSearchResult {
    CassSearchResult {
        query: Some("error mismatched types".to_string()),
        limit: Some(hit_count),
        offset: Some(0),
        count: Some(hit_count),
        total_matches: Some(hit_count * 3),
        hits: (0..hit_count).map(make_search_hit).collect(),
        max_tokens: Some(4096),
        request_id: Some("bench-req-001".to_string()),
        cursor: None,
        hits_clamped: Some(false),
        extra: HashMap::new(),
    }
}

fn make_search_result_json(hit_count: usize) -> String {
    let result = make_search_result(hit_count);
    serde_json::to_string(&result).expect("serialize search result")
}

fn make_session(message_count: usize) -> CassSession {
    CassSession {
        session_id: Some("bench-session-001".to_string()),
        agent: Some("claude_code".to_string()),
        project_path: Some("/workspace/frankenterm".to_string()),
        started_at: Some("2026-04-19T10:00:00Z".to_string()),
        ended_at: Some("2026-04-19T11:30:00Z".to_string()),
        messages: (0..message_count)
            .map(|i| CassMessage {
                role: Some(if i % 2 == 0 { "user" } else { "assistant" }.to_string()),
                content: Some(format!(
                    "Message {i}: discussing the implementation of feature X with various considerations"
                )),
                timestamp: Some(format!(
                    "2026-04-19T10:{:02}:{:02}Z",
                    i / 60,
                    i % 60
                )),
                token_count: Some(100 + (i as u64 % 500)),
                extra: HashMap::new(),
            })
            .collect(),
        extra: HashMap::new(),
    }
}

/// Format a CassSearchHit as a hint string (mirrors HandleOnErrorCassSearch logic).
fn format_hit_as_hint(hit: &CassSearchHit) -> Option<String> {
    let content = hit.content.as_deref()?.trim();
    if content.is_empty() {
        return None;
    }
    let path = hit.source_path.as_deref().unwrap_or("unknown");
    let line = hit.line_number.unwrap_or(0);
    Some(format!("{path}:{line} - {content}"))
}

// ---------------------------------------------------------------------------
// Benchmarks: CassSearchResult deserialization
// ---------------------------------------------------------------------------

fn bench_search_result_deser(c: &mut Criterion) {
    let mut group = c.benchmark_group("cass_search_ops/search_result_deser");
    group.measurement_time(Duration::from_secs(5));

    for &hit_count in &[10usize, 50, 200] {
        let json = make_search_result_json(hit_count);
        group.throughput(Throughput::Bytes(json.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("hits", hit_count),
            &json,
            |b, json| {
                b.iter(|| {
                    let result: CassSearchResult =
                        serde_json::from_str(black_box(json)).expect("deser");
                    black_box(result.hits.len());
                });
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmarks: parse_cass_timestamp_ms
// ---------------------------------------------------------------------------

fn bench_timestamp_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("cass_search_ops/timestamp_parse");
    group.measurement_time(Duration::from_secs(5));

    let cases: Vec<(&str, &str)> = vec![
        ("epoch_ms", "1700000000000"),
        ("epoch_s", "1700000000"),
        ("rfc3339", "2026-04-19T12:00:00Z"),
        ("rfc3339_offset", "2026-04-19T12:00:00-04:00"),
        ("whitespace", "  1700000000000  "),
        ("empty", ""),
    ];

    let batch_size = 1000u64;
    group.throughput(Throughput::Elements(batch_size));

    for (label, input) in &cases {
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            input,
            |b, input| {
                b.iter(|| {
                    for _ in 0..batch_size {
                        black_box(parse_cass_timestamp_ms(black_box(input)));
                    }
                });
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmarks: Hint formatting
// ---------------------------------------------------------------------------

fn bench_hint_format(c: &mut Criterion) {
    let mut group = c.benchmark_group("cass_search_ops/hint_format");
    group.measurement_time(Duration::from_secs(5));

    for &hit_count in &[10usize, 50, 200] {
        let hits: Vec<CassSearchHit> = (0..hit_count).map(make_search_hit).collect();
        group.throughput(Throughput::Elements(hit_count as u64));
        group.bench_with_input(
            BenchmarkId::new("hits", hit_count),
            &hits,
            |b, hits| {
                b.iter(|| {
                    let hints: Vec<String> = hits
                        .iter()
                        .filter_map(|h| format_hit_as_hint(black_box(h)))
                        .collect();
                    black_box(hints.len());
                });
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmarks: Session summary computation
// ---------------------------------------------------------------------------

fn bench_session_summary(c: &mut Criterion) {
    let mut group = c.benchmark_group("cass_search_ops/session_summary");
    group.measurement_time(Duration::from_secs(5));

    for &msg_count in &[10usize, 50, 100] {
        let session = make_session(msg_count);
        group.throughput(Throughput::Elements(msg_count as u64));
        group.bench_with_input(
            BenchmarkId::new("messages", msg_count),
            &session,
            |b, session| {
                b.iter(|| {
                    // Compute summary: total tokens, message count, time span
                    let total_tokens: u64 = session
                        .messages
                        .iter()
                        .filter_map(|m| m.token_count)
                        .sum();
                    let first_ts = session
                        .messages
                        .first()
                        .and_then(|m| m.timestamp.as_deref())
                        .and_then(parse_cass_timestamp_ms);
                    let last_ts = session
                        .messages
                        .last()
                        .and_then(|m| m.timestamp.as_deref())
                        .and_then(parse_cass_timestamp_ms);
                    let summary = CassSessionSummary {
                        total_tokens: Some(total_tokens as i64),
                        input_tokens: None,
                        output_tokens: None,
                        message_count: session.messages.len(),
                        session_started_at_ms: first_ts,
                        session_ended_at_ms: last_ts,
                        first_message_at_ms: first_ts,
                        ..Default::default()
                    };
                    black_box(summary.total_tokens);
                });
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmarks: Serde roundtrip
// ---------------------------------------------------------------------------

fn bench_serde_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("cass_search_ops/serde_roundtrip");
    group.measurement_time(Duration::from_secs(5));

    for &hit_count in &[10usize, 50, 200] {
        let result = make_search_result(hit_count);
        let json = serde_json::to_string(&result).expect("serialize");
        group.throughput(Throughput::Bytes(json.len() as u64));

        group.bench_with_input(
            BenchmarkId::new("roundtrip", hit_count),
            &result,
            |b, result| {
                b.iter(|| {
                    let json = serde_json::to_string(black_box(result)).expect("ser");
                    let back: CassSearchResult =
                        serde_json::from_str(black_box(&json)).expect("deser");
                    black_box(back.hits.len());
                });
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Suite
// ---------------------------------------------------------------------------

fn bench_suite(c: &mut Criterion) {
    bench_search_result_deser(c);
    bench_timestamp_parse(c);
    bench_hint_format(c);
    bench_session_summary(c);
    bench_serde_roundtrip(c);
    bench_common::emit_bench_artifacts("cass_search_ops", BUDGETS);
}

criterion_group!(benches, bench_suite);
criterion_main!(benches);
