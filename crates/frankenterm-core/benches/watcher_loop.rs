//! Benchmarks for watcher loop overhead.
//!
//! This measures the per-pane overhead when the watcher is idle (no new output).
//! These are the operations that happen on each poll tick for each pane.
//!
//! Performance budgets:
//! - Watcher loop overhead (idle): **< 100µs per pane check**

use base64::Engine as _;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use frankenterm_core::config::{PaneFilterConfig, PaneFilterRule};
use frankenterm_core::ingest::PaneFingerprint;
use frankenterm_core::wezterm::PaneInfo;
use serde::Deserialize;
use std::hint::black_box;
use std::process::Command;
use std::time::Duration;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "watcher_loop_idle",
        budget: "< 100µs per pane check (idle, no new output)",
    },
    bench_common::BenchBudget {
        name: "watcher_native_event/native_decode_dispatch",
        budget: "in-process native wire decode baseline (lower than Lua proxy simulation)",
    },
    bench_common::BenchBudget {
        name: "watcher_native_event/lua_cli_proxy_sim",
        budget: "legacy Lua->CLI proxy simulation includes per-event process spawn overhead",
    },
];

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum NativeWireBenchEvent {
    PaneOutput {
        pane_id: u64,
        data_b64: String,
        ts: u64,
    },
    #[serde(other)]
    Other,
}

fn decode_native_pane_output(line: &str) -> Option<(u64, usize, u64)> {
    let event: NativeWireBenchEvent = serde_json::from_str(line).ok()?;
    match event {
        NativeWireBenchEvent::PaneOutput {
            pane_id,
            data_b64,
            ts,
        } => {
            let payload = base64::engine::general_purpose::STANDARD
                .decode(data_b64.as_bytes())
                .ok()?;
            Some((pane_id, payload.len(), ts))
        }
        NativeWireBenchEvent::Other => None,
    }
}

fn spawn_noop_process() -> bool {
    #[cfg(windows)]
    {
        Command::new("cmd")
            .args(["/C", "exit", "0"])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    #[cfg(not(windows))]
    {
        Command::new("sh")
            .args(["-c", "true"])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}

fn simulate_lua_cli_proxy(pane_id: u64, pane_line: &str) -> bool {
    let serialized_line = serde_json::to_string(pane_line).unwrap_or_else(|_| "\"\"".to_string());
    let envelope =
        format!("{{\"event\":\"status_update\",\"pane_id\":{pane_id},\"line\":{serialized_line}}}");
    let envelope_len = envelope.len();
    envelope_len > 0 && spawn_noop_process()
}

/// Create a realistic test pane.
fn test_pane(pane_id: u64) -> PaneInfo {
    PaneInfo {
        pane_id,
        tab_id: 1,
        window_id: 1,
        domain_id: None,
        domain_name: Some("local".to_string()),
        workspace: None,
        size: None,
        rows: None,
        cols: None,
        title: Some("zsh".to_string()),
        cwd: Some("/home/user/projects/frankenterm".to_string()),
        tty_name: None,
        cursor_x: Some(0),
        cursor_y: Some(24),
        cursor_visibility: None,
        left_col: None,
        top_row: None,
        is_active: true,
        is_zoomed: false,
        extra: std::collections::HashMap::new(),
    }
}

/// Create filter config with typical rules.
fn typical_filter() -> PaneFilterConfig {
    PaneFilterConfig {
        include: vec![],
        exclude: vec![
            PaneFilterRule {
                id: "exclude-htop".to_string(),
                domain: None,
                title: Some("*htop*".to_string()),
                cwd: None,
            },
            PaneFilterRule {
                id: "exclude-vim".to_string(),
                domain: None,
                title: Some("*vim*".to_string()),
                cwd: None,
            },
            PaneFilterRule {
                id: "exclude-private".to_string(),
                domain: None,
                title: None,
                cwd: Some("*/private/*".to_string()),
            },
        ],
    }
}

/// Create filter config with many rules (stress test).
fn heavy_filter() -> PaneFilterConfig {
    let mut exclude = Vec::with_capacity(20);
    for i in 0..20 {
        exclude.push(PaneFilterRule {
            id: format!("rule-{i}"),
            domain: None,
            title: Some(format!("*pattern{i}*")),
            cwd: None,
        });
    }
    PaneFilterConfig {
        include: vec![],
        exclude,
    }
}

fn bench_pane_filter(c: &mut Criterion) {
    let mut group = c.benchmark_group("watcher_pane_filter");

    let typical = typical_filter();
    let heavy = heavy_filter();

    // Budget: < 100µs total per pane check
    // Filter check should be a small fraction of that
    group.bench_function("typical_filter_no_match", |b| {
        b.iter(|| typical.check_pane("local", "zsh", "/home/user/projects"));
    });

    group.bench_function("typical_filter_match", |b| {
        b.iter(|| typical.check_pane("local", "htop - ubuntu", "/home/user"));
    });

    group.bench_function("heavy_filter_no_match", |b| {
        b.iter(|| heavy.check_pane("local", "bash", "/home/user/work"));
    });

    group.bench_function("heavy_filter_match", |b| {
        b.iter(|| heavy.check_pane("local", "xpattern10x", "/tmp"));
    });

    group.finish();
}

fn bench_pane_fingerprint(c: &mut Criterion) {
    let mut group = c.benchmark_group("watcher_fingerprint");

    let pane = test_pane(1);

    // Fingerprint without content (used for comparison)
    group.bench_function("fingerprint_without_content", |b| {
        b.iter(|| PaneFingerprint::without_content(&pane));
    });

    // Fingerprint with typical content
    let typical_content = "$ ls -la\ntotal 64\ndrwxr-xr-x  10 user  staff\n";
    group.bench_function("fingerprint_with_content_small", |b| {
        b.iter(|| PaneFingerprint::new(&pane, Some(typical_content)));
    });

    // Fingerprint with larger content
    let large_content = typical_content.repeat(100);
    group.bench_function("fingerprint_with_content_large", |b| {
        b.iter(|| PaneFingerprint::new(&pane, Some(&large_content)));
    });

    group.finish();
}

fn bench_pane_check_combined(c: &mut Criterion) {
    let mut group = c.benchmark_group("watcher_combined_check");

    let filter = typical_filter();
    let pane = test_pane(1);
    let content = "$ cargo build\n   Compiling frankenterm-core v0.1.0\n    Finished dev\n";

    // Combined operation: filter + fingerprint (what happens each poll tick)
    // Budget: < 100µs total
    group.bench_function("filter_and_fingerprint", |b| {
        b.iter(|| {
            let _excluded = filter.check_pane(
                pane.domain_name.as_deref().unwrap_or("local"),
                pane.title.as_deref().unwrap_or(""),
                pane.cwd.as_deref().unwrap_or(""),
            );
            let _fp = PaneFingerprint::new(&pane, Some(content));
        });
    });

    // Multiple panes (simulate checking 10 panes)
    let panes: Vec<_> = (0..10).map(test_pane).collect();
    group.bench_function("check_10_panes", |b| {
        b.iter(|| {
            for pane in &panes {
                let _excluded = filter.check_pane(
                    pane.domain_name.as_deref().unwrap_or("local"),
                    pane.title.as_deref().unwrap_or(""),
                    pane.cwd.as_deref().unwrap_or(""),
                );
                let _fp = PaneFingerprint::without_content(pane);
            }
        });
    });

    // Stress test: 50 panes
    let many_panes: Vec<_> = (0..50).map(test_pane).collect();
    group.bench_with_input(
        BenchmarkId::new("check_many_panes", 50),
        &many_panes,
        |b, panes| {
            b.iter(|| {
                for pane in panes {
                    let _excluded = filter.check_pane(
                        pane.domain_name.as_deref().unwrap_or("local"),
                        pane.title.as_deref().unwrap_or(""),
                        pane.cwd.as_deref().unwrap_or(""),
                    );
                    let _fp = PaneFingerprint::without_content(pane);
                }
            });
        },
    );

    group.finish();
}

fn bench_native_event_latency_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("watcher_native_event");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(3));

    let wire_payload = format!(
        r#"{{"type":"pane_output","pane_id":7,"data_b64":"{}","ts":12345}}"#,
        base64::engine::general_purpose::STANDARD
            .encode(b"usage reached: run /compact to continue")
    );

    group.bench_function("native_decode_dispatch", |b| {
        b.iter(|| {
            let decoded = decode_native_pane_output(black_box(&wire_payload)).expect("decode");
            black_box(decoded);
        });
    });

    let pane_line = "usage reached. compact requested";
    group.bench_function("lua_cli_proxy_sim", |b| {
        b.iter(|| {
            let ok = simulate_lua_cli_proxy(black_box(7), black_box(pane_line));
            black_box(ok);
        });
    });

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("watcher_loop", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_pane_filter,
        bench_pane_fingerprint,
        bench_pane_check_combined,
        bench_native_event_latency_comparison
);
criterion_main!(benches);
