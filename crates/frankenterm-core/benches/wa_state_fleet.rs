//! Full wa.state envelope construction at fleet scale (ft-3r0n4).
//!
//! ## Why this exists
//!
//! `mcp_response.rs` benches the MCP MANIFEST list (tools[]). The
//! `toon_encoding.rs` bench (sibling — ft-0zoq3) covers the encoder
//! hot path on `wa_state_payload` at varying pane counts. Neither
//! measures the FULL envelope-construction pipeline an `ft robot
//! state` / MCP `wa.state` call traverses at fleet scale:
//!
//!   1. Synthetic fleet (this bench: 10/50/200 panes; production:
//!      mux RPC retrieves PaneInfo).
//!   2. **Redactor sweep** on `title`, `cwd`, `ignore_reason` (per
//!      `mcp_tools.rs:1253 redact_mcp_pane_state_fields`).
//!   3. Envelope wrapping (ok=true, data, elapsed_ms, version, now).
//!   4. Serialization (JSON or TOON).
//!
//! `toon_encoding` covers step 4 in isolation. This bench measures
//! the FULL pipeline (steps 2 → 4) so we can attribute cost across
//! the redactor / serializer split — the question my profiling
//! audit (ft-3r0n4) named: "is JSON 2× slower than TOON here?
//! does the redactor dominate? does extra-HashMap allocation
//! dominate?"
//!
//! ## Workloads
//!
//! Three pipelines × three pane counts (10, 50, 200) × two output
//! formats (JSON, TOON):
//!
//! | Pipeline           | Steps included         |
//! |--------------------|------------------------|
//! | construct_only     | 1 (build Vec<Value>)   |
//! | construct_redact   | 1 + 2                  |
//! | full_envelope_json | 1 + 2 + 3 + 4 (JSON)   |
//! | full_envelope_toon | 1 + 2 + 3 + 4 (TOON)   |
//!
//! Subtraction of group medians attributes:
//!   redactor_cost = (construct_redact - construct_only)
//!   serialize_json = (full_envelope_json - construct_redact)
//!   serialize_toon = (full_envelope_toon - construct_redact)
//!   format_overhead = serialize_json vs serialize_toon
//!
//! ## Adversarial fields
//!
//! Half the synthetic panes carry an embedded fake API key in
//! `cwd` (matching the `OPENAI_API_KEY=sk-fake...` pattern the
//! redactor catches). This forces the redactor's regex to actually
//! fire — measuring "redactor as no-op" instead of "redactor on
//! adversarial paste" would understate the cost.
//!
//! ## Output
//!
//! Hypothesis vs measured documented at
//! `docs/perf-ledger/wa-state-fleet.md`.

use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::redactor::Redactor;
use serde_json::{Value, json};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "wa_state_fleet_construct_only",
        budget: "build Vec<Value> for fleet of 10/50/200 panes (no redact, no encode)",
    },
    bench_common::BenchBudget {
        name: "wa_state_fleet_construct_redact",
        budget: "construct + redactor sweep at 10/50/200 panes",
    },
    bench_common::BenchBudget {
        name: "wa_state_fleet_full_envelope_json",
        budget: "construct + redact + envelope wrap + JSON encode at 10/50/200 panes",
    },
    bench_common::BenchBudget {
        name: "wa_state_fleet_full_envelope_toon",
        budget: "construct + redact + envelope wrap + TOON encode at 10/50/200 panes",
    },
];

const PANE_COUNTS: &[usize] = &[10, 50, 200];

/// Synthetic pane shape mirroring the wa.state response. Half the
/// panes carry a fake API-key-shaped string in `cwd` so the redactor
/// regex actually fires (not a no-op measurement).
fn pane(index: usize) -> Value {
    let cwd = if index % 2 == 0 {
        // Adversarial: fake API key embedded in path. Matches the
        // OpenAI prefix `sk-` regex in redactor.rs:18-19. Synthetic
        // — non-functional bytes; the test exists only to force the
        // redactor's regex to do real work.
        format!(
            "/Users/jemanuel/projects/frankenterm/session-{index:04}/.env-sk-fakeABCDEF1234567890abcdefghij"
        )
    } else {
        format!("/Users/jemanuel/projects/frankenterm/session-{index:04}")
    };
    json!({
        "pane_id": index as u64,
        "pane_uuid": format!("pane-uuid-{index:04}"),
        "tab_id": index / 4,
        "window_id": index / 16,
        "domain": if index % 7 == 0 { "ssh:builder" } else { "local" },
        "title": format!("cod_{index}_worker"),
        "cwd": cwd,
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

fn construct_panes(panes: usize) -> Vec<Value> {
    (0..panes).map(pane).collect()
}

/// Mirror of `redact_mcp_pane_state_fields` from
/// `crates/frankenterm-core/src/mcp_tools.rs:1253`. Walks the pane
/// vector, redacts the three string-bearing fields the production
/// path scrubs (title, cwd, ignore_reason).
fn redact_pane_state_fields(redactor: &Redactor, panes: &mut [Value]) {
    for state in panes.iter_mut() {
        let Some(obj) = state.as_object_mut() else {
            continue;
        };
        for field in ["title", "cwd", "ignore_reason"] {
            if let Some(Value::String(s)) = obj.get(field) {
                let redacted = redactor.redact(s);
                obj.insert(field.to_string(), Value::String(redacted));
            }
        }
    }
}

fn wrap_envelope(panes: Vec<Value>, panes_count: usize) -> Value {
    json!({
        "ok": true,
        "data": {
            "panes": panes,
            "tail_lines": 200,
            "escapes_included": false
        },
        "elapsed_ms": 7,
        "version": "bench",
        "now": 1_777_200_000_000u64,
        "mcp_version": "2025-03-26",
        "panes_count": panes_count
    })
}

// ── construct_only ─────────────────────────────────────────────

fn bench_construct_only(c: &mut Criterion) {
    let mut group = c.benchmark_group("wa_state_fleet_construct_only");
    group.sample_size(50);
    for &count in PANE_COUNTS {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter(|| {
                let panes = construct_panes(black_box(count));
                black_box(panes);
            });
        });
    }
    group.finish();
}

// ── construct + redact ─────────────────────────────────────────

fn bench_construct_redact(c: &mut Criterion) {
    let mut group = c.benchmark_group("wa_state_fleet_construct_redact");
    group.sample_size(50);
    let redactor = Redactor::new();
    for &count in PANE_COUNTS {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter_batched(
                || construct_panes(count),
                |mut panes| {
                    redact_pane_state_fields(&redactor, &mut panes);
                    black_box(panes);
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

// ── full envelope JSON ─────────────────────────────────────────

fn bench_full_envelope_json(c: &mut Criterion) {
    let mut group = c.benchmark_group("wa_state_fleet_full_envelope_json");
    group.sample_size(40);
    let redactor = Redactor::new();
    for &count in PANE_COUNTS {
        // Pre-build once to size the throughput estimate.
        let mut sample_panes = construct_panes(count);
        redact_pane_state_fields(&redactor, &mut sample_panes);
        let envelope = wrap_envelope(sample_panes, count);
        let json_len = serde_json::to_vec(&envelope)
            .expect("benchmark envelope serializes")
            .len();
        group.throughput(Throughput::Bytes(json_len as u64));

        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter_batched(
                || construct_panes(count),
                |mut panes| {
                    redact_pane_state_fields(&redactor, &mut panes);
                    let envelope = wrap_envelope(panes, count);
                    let bytes = serde_json::to_vec(black_box(&envelope))
                        .expect("serialize JSON envelope");
                    black_box(bytes);
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

// ── full envelope TOON ─────────────────────────────────────────

fn bench_full_envelope_toon(c: &mut Criterion) {
    let mut group = c.benchmark_group("wa_state_fleet_full_envelope_toon");
    group.sample_size(40);
    let redactor = Redactor::new();
    for &count in PANE_COUNTS {
        let mut sample_panes = construct_panes(count);
        redact_pane_state_fields(&redactor, &mut sample_panes);
        let envelope = wrap_envelope(sample_panes, count);
        let toon_len = toon_rust::encode(envelope.clone(), None).len();
        group.throughput(Throughput::Bytes(toon_len as u64));

        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter_batched(
                || construct_panes(count),
                |mut panes| {
                    redact_pane_state_fields(&redactor, &mut panes);
                    let envelope = wrap_envelope(panes, count);
                    let text = toon_rust::encode(black_box(envelope), None);
                    black_box(text);
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_wa_state_fleet(c: &mut Criterion) {
    bench_common::emit_bench_artifacts("wa_state_fleet", BUDGETS);
    bench_construct_only(c);
    bench_construct_redact(c);
    bench_full_envelope_json(c);
    bench_full_envelope_toon(c);
}

criterion_group!(benches, bench_wa_state_fleet);
criterion_main!(benches);
