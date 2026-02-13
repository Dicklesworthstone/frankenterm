//! Criterion benchmarks for streaming subscription behavior.
//!
//! Required benchmark targets:
//! - `bench_subscription_first_message_latency`
//! - `bench_subscription_steady_state_throughput`
//! - `bench_multi_subscriber_fanout`
//! - `bench_redaction_overhead`

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use frankenterm_core::events::{Event, EventBus};
use frankenterm_core::patterns::{AgentType, Detection, Severity};
use frankenterm_core::policy::Redactor;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "subscription_streaming/bench_subscription_first_message_latency",
        budget: "time from subscription + publish to first delivered message",
    },
    bench_common::BenchBudget {
        name: "subscription_streaming/bench_subscription_steady_state_throughput",
        budget: "sustained messages/sec delivery to single subscriber",
    },
    bench_common::BenchBudget {
        name: "subscription_streaming/bench_multi_subscriber_fanout",
        budget: "fanout latency/throughput for 1, 10, and 50 subscribers",
    },
    bench_common::BenchBudget {
        name: "subscription_streaming/bench_redaction_overhead",
        budget: "per-message redaction overhead during streaming",
    },
];

fn sample_detection(i: u64) -> Detection {
    Detection {
        rule_id: format!("core.bench:{i}"),
        agent_type: AgentType::Codex,
        event_type: "usage_reached".to_string(),
        severity: Severity::Warning,
        confidence: 1.0,
        extracted: serde_json::json!({
            "token": "sk-bench-bench-bench-bench-bench-bench-bench-bench-bench-bench",
            "index": i,
        }),
        matched_text: format!(
            "usage reached with token sk-bench-bench-bench-bench-bench-bench-bench-bench #{i}"
        ),
        span: (0, 0),
    }
}

fn bench_subscription_first_message_latency(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("create runtime");
    let mut group = c.benchmark_group("subscription_streaming");

    group.bench_function("bench_subscription_first_message_latency", |b| {
        b.to_async(&rt).iter(|| async {
            let bus = EventBus::new(256);
            let mut sub = bus.subscribe_deltas();

            let start = std::time::Instant::now();
            let _ = bus.publish(Event::SegmentCaptured {
                pane_id: 1,
                seq: 1,
                content_len: 24,
            });
            let received = sub.recv().await.expect("receive first message");
            black_box(received);
            black_box(start.elapsed());
        });
    });

    group.finish();
}

fn bench_subscription_steady_state_throughput(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("create runtime");
    let mut group = c.benchmark_group("subscription_streaming");

    group.bench_function("bench_subscription_steady_state_throughput", |b| {
        b.to_async(&rt).iter(|| async {
            let bus = EventBus::new(2048);
            let mut sub = bus.subscribe_deltas();

            for i in 0..2_000_u64 {
                let _ = bus.publish(Event::SegmentCaptured {
                    pane_id: 2,
                    seq: i,
                    content_len: 48,
                });
                let event = sub.recv().await.expect("steady-state recv");
                black_box(event);
            }
        });
    });

    group.finish();
}

fn bench_multi_subscriber_fanout(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("create runtime");
    let mut group = c.benchmark_group("subscription_streaming");

    for subscribers in [1_usize, 10, 50] {
        group.bench_with_input(
            BenchmarkId::new("bench_multi_subscriber_fanout", subscribers),
            &subscribers,
            |b, subscribers| {
                b.to_async(&rt).iter(|| async move {
                    let bus = EventBus::new(4096);
                    let mut subs: Vec<_> = (0..*subscribers)
                        .map(|_| bus.subscribe_detections())
                        .collect();

                    for i in 0..256_u64 {
                        let detection = sample_detection(i);
                        let _ = bus.publish(Event::PatternDetected {
                            pane_id: 42,
                            pane_uuid: None,
                            detection,
                            event_id: None,
                        });

                        for sub in &mut subs {
                            let event = sub.recv().await.expect("fanout recv");
                            black_box(&event);
                        }
                    }
                });
            },
        );
    }

    group.finish();
}

fn bench_redaction_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("subscription_streaming");
    let redactor = Redactor::new();
    let payload = r#"{"token":"sk-redact-redact-redact-redact-redact-redact-redact-redact","pane":7,"message":"normal text"}"#;

    group.bench_function("bench_redaction_overhead", |b| {
        b.iter(|| {
            let out = redactor.redact(black_box(payload));
            black_box(out);
        });
    });

    group.finish();
}

fn bench_suite(c: &mut Criterion) {
    bench_subscription_first_message_latency(c);
    bench_subscription_steady_state_throughput(c);
    bench_multi_subscriber_fanout(c);
    bench_redaction_overhead(c);
    bench_common::emit_bench_artifacts("subscription_streaming", BUDGETS);
}

criterion_group!(benches, bench_suite);
criterion_main!(benches);
