//! Round-5 A1 isolation benches for gated pattern-engine optimizations.
//!
//! The orchestrator runs this file twice per gate: baseline without the feature
//! and candidate with the feature. Keep benchmark IDs stable across arms.

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::config::PatternsConfig;
use frankenterm_core::patterns::{
    AgentType, DetectionContext, PatternEngine, PatternPack, RuleDef, Severity,
};
use std::fmt::Write as _;
use std::hint::black_box;
use std::io::Write as _;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "q5_teddy_low_match",
        budget: "feature teddy-prefilter should reject low-match chunks before Aho/regex",
    },
    bench_common::BenchBudget {
        name: "q6_fingerprint_dedup_churn",
        budget: "feature patterns-fingerprint-dedup should reduce high-churn dedup overhead",
    },
    bench_common::BenchBudget {
        name: "m5_mphf_chatty_anchor_routing",
        budget: "feature patterns-mphf-dispatch should reduce chatty anchor routing overhead",
    },
];

const Q5_CHUNKS: usize = 512;
const Q6_DISTINCT_KEYS: usize = 6_144;
const M5_RULES: usize = 192;
const M5_REPETITIONS: usize = 24;

fn rule(id: String, event_type: &str, anchor: String, regex: Option<String>) -> RuleDef {
    RuleDef {
        id,
        agent_type: AgentType::Codex,
        event_type: event_type.to_string(),
        severity: Severity::Info,
        anchors: vec![anchor],
        regex,
        description: "Round-5 A1 benchmark rule".to_string(),
        remediation: None,
        workflow: None,
        manual_fix: None,
        preview_command: None,
        learn_more_url: None,
    }
}

fn engine_from_config(config: &PatternsConfig, label: &str) -> PatternEngine {
    match PatternEngine::from_config(config) {
        Ok(engine) => engine,
        Err(err) => {
            let mut stderr = std::io::stderr().lock();
            let _ = writeln!(stderr, "failed to build {label} pattern engine: {err}");
            std::process::abort();
        }
    }
}

fn engine_from_pack(pack: PatternPack, label: &str) -> PatternEngine {
    match PatternEngine::with_packs(vec![pack]) {
        Ok(engine) => engine,
        Err(err) => {
            let mut stderr = std::io::stderr().lock();
            let _ = writeln!(stderr, "failed to build {label} pattern engine: {err}");
            std::process::abort();
        }
    }
}

fn usize_to_u64(value: usize) -> u64 {
    match u64::try_from(value) {
        Ok(value) => value,
        Err(_) => u64::MAX,
    }
}

fn q5_teddy_engine() -> PatternEngine {
    let mut config = PatternsConfig::default();
    config.quick_reject_enabled = false;
    config.user_packs_enabled = false;
    engine_from_config(&config, "q5_teddy_low_match")
}

fn q5_low_match_chunks() -> Vec<String> {
    let mut chunks = Vec::with_capacity(Q5_CHUNKS);
    for idx in 0..Q5_CHUNKS {
        let mut chunk = String::with_capacity(160);
        if idx % 64 == 0 {
            let _ = writeln!(
                chunk,
                "round5 pane {idx}: Warning: less than 10% of your 20h limit remaining."
            );
            chunk.push_str("retry metadata follows, but this chunk is intentionally rare.\n");
        } else {
            let _ = writeln!(
                chunk,
                "round5 pane {idx}: heartbeat ok; compile queue steady; tokens sampled; \
                 no recognizable agent limit, compaction, rate, or session banner here."
            );
        }
        chunks.push(chunk);
    }
    chunks
}

fn q6_dedup_engine() -> PatternEngine {
    let pack = PatternPack::new(
        "builtin:round5_q6_dedup",
        "1.0.0",
        vec![rule(
            "codex.round5_q6_dedup".to_string(),
            "round5.dedup",
            "ROUND5_DEDUP_".to_string(),
            Some(r"ROUND5_DEDUP_(?P<seq>\d+)".to_string()),
        )],
    );
    engine_from_pack(pack, "q6_fingerprint_dedup_churn")
}

fn q6_dedup_chunks() -> Vec<String> {
    let mut chunks = Vec::with_capacity(Q6_DISTINCT_KEYS * 2);
    for pass in 0..2 {
        for seq in 0..Q6_DISTINCT_KEYS {
            let mut chunk = String::with_capacity(56);
            let _ = writeln!(
                chunk,
                "worker={pass} event=ROUND5_DEDUP_{seq:05} status=seen-again"
            );
            chunks.push(chunk);
        }
    }
    chunks
}

fn m5_mphf_engine() -> PatternEngine {
    let mut rules = Vec::with_capacity(M5_RULES);
    for idx in 0..M5_RULES {
        let mut rule_id = String::with_capacity(28);
        let _ = write!(rule_id, "codex.round5_m5_route_{idx}");
        let mut anchor = String::with_capacity(22);
        let _ = write!(anchor, "ROUND5_MPHF_ANCHOR_{idx:03}");
        rules.push(rule(rule_id, "round5.mphf", anchor, None));
    }
    let pack = PatternPack::new("builtin:round5_m5_mphf", "1.0.0", rules);
    engine_from_pack(pack, "m5_mphf_chatty_anchor_routing")
}

fn m5_chatty_payload() -> String {
    let mut payload = String::with_capacity(M5_REPETITIONS * M5_RULES * 48);
    for pass in 0..M5_REPETITIONS {
        for idx in 0..M5_RULES {
            payload.push_str("agent=");
            let _ = write!(payload, "{}", idx % 16);
            payload.push_str(" pass=");
            let _ = write!(payload, "{pass}");
            payload.push(' ');
            let _ = write!(payload, "ROUND5_MPHF_ANCHOR_{idx:03}");
            payload.push_str(" completed step\n");
        }
    }
    payload
}

fn bench_q5_teddy_low_match(c: &mut Criterion) {
    let engine = q5_teddy_engine();
    let chunks = q5_low_match_chunks();
    let bytes: usize = chunks.iter().map(String::len).sum();
    black_box(cfg!(feature = "teddy-prefilter"));

    let mut group = c.benchmark_group("round5_patterns_a1/q5_teddy_low_match");
    group.throughput(Throughput::Bytes(usize_to_u64(bytes)));
    group.bench_function(BenchmarkId::from_parameter("512_chunks"), |b| {
        b.iter(|| {
            let mut detections = 0usize;
            for chunk in &chunks {
                detections += engine.detect(black_box(chunk)).len();
            }
            black_box(detections)
        });
    });
    group.finish();
}

fn bench_q6_fingerprint_dedup_churn(c: &mut Criterion) {
    let engine = q6_dedup_engine();
    let chunks = q6_dedup_chunks();
    let bytes: usize = chunks.iter().map(String::len).sum();
    black_box(cfg!(feature = "patterns-fingerprint-dedup"));

    let mut group = c.benchmark_group("round5_patterns_a1/q6_fingerprint_dedup_churn");
    group.throughput(Throughput::Bytes(usize_to_u64(bytes)));
    group.bench_function(BenchmarkId::from_parameter("6144_keys_x2"), |b| {
        b.iter_batched(
            || DetectionContext::with_agent_type(AgentType::Codex),
            |mut context| {
                let mut detections = 0usize;
                for chunk in &chunks {
                    detections += engine
                        .detect_with_context(black_box(chunk), &mut context)
                        .len();
                }
                black_box((detections, context.seen_count()))
            },
            BatchSize::LargeInput,
        );
    });
    group.finish();
}

fn bench_m5_mphf_chatty_anchor_routing(c: &mut Criterion) {
    let engine = m5_mphf_engine();
    let payload = m5_chatty_payload();
    black_box(cfg!(feature = "patterns-mphf-dispatch"));

    let mut group = c.benchmark_group("round5_patterns_a1/m5_mphf_chatty_anchor_routing");
    group.throughput(Throughput::Bytes(usize_to_u64(payload.len())));
    group.bench_function(BenchmarkId::from_parameter("192_anchors_x24"), |b| {
        b.iter(|| black_box(engine.detect(black_box(&payload)).len()));
    });
    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("round5_patterns_a1", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_q5_teddy_low_match,
        bench_q6_fingerprint_dedup_churn,
        bench_m5_mphf_chatty_anchor_routing
);
criterion_main!(benches);
