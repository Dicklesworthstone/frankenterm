//! Criterion benchmarks for config parsing and pane filter/priority hot paths.
//!
//! These functions are called on every ingest event, so their performance
//! directly impacts observation loop latency.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use frankenterm_core::config::{Config, PaneFilterConfig, PaneFilterRule, PanePriorityConfig};
use std::hint::black_box;

// =============================================================================
// Minimal TOML for parsing benchmarks
// =============================================================================

fn minimal_toml() -> &'static str {
    "[general]\nlog_level = \"info\"\n"
}

fn medium_toml() -> String {
    let mut s = String::with_capacity(2048);
    s.push_str("[general]\nlog_level = \"debug\"\n\n");
    s.push_str("[ingest]\npoll_interval_ms = 100\nmax_concurrent_captures = 20\n\n");
    s.push_str("[storage]\ndata_dir = \"/tmp/ft-bench\"\nmax_db_size_mb = 512\n\n");
    s.push_str("[ingest.panes]\n\n");
    s.push_str("[[ingest.panes.exclude]]\n");
    s.push_str("id = \"skip-vim\"\ntitle = \"re:^vim\\\\b\"\n\n");
    s.push_str("[[ingest.panes.exclude]]\n");
    s.push_str("id = \"skip-ssh\"\ndomain = \"SSH:*\"\n\n");
    s.push_str("[[ingest.panes.include]]\n");
    s.push_str("id = \"local-only\"\ndomain = \"local\"\n\n");
    s
}

fn large_toml() -> String {
    let mut s = medium_toml();
    // Add 50 exclude rules to stress filter matching
    for i in 0..50 {
        s.push_str(&format!(
            "[[ingest.panes.exclude]]\nid = \"rule-{i}\"\ntitle = \"re:pattern-{i}.*end\"\n\n"
        ));
    }
    s
}

// =============================================================================
// Benchmark: Config::from_toml parsing
// =============================================================================

fn bench_config_parse(c: &mut Criterion) {
    let minimal = minimal_toml().to_string();
    let medium = medium_toml();
    let large = large_toml();

    let mut group = c.benchmark_group("config/parse");

    group.bench_with_input(BenchmarkId::new("toml", "minimal"), &minimal, |b, toml| {
        b.iter(|| Config::from_toml(black_box(toml)).unwrap());
    });

    group.bench_with_input(BenchmarkId::new("toml", "medium"), &medium, |b, toml| {
        b.iter(|| Config::from_toml(black_box(toml)).unwrap());
    });

    group.bench_with_input(
        BenchmarkId::new("toml", "large_50_rules"),
        &large,
        |b, toml| {
            b.iter(|| Config::from_toml(black_box(toml)).unwrap());
        },
    );

    group.finish();
}

// =============================================================================
// Benchmark: check_pane filter matching
// =============================================================================

fn make_filter_config(n_exclude: usize) -> PaneFilterConfig {
    let mut config = PaneFilterConfig::default();
    for i in 0..n_exclude {
        config.exclude.push(PaneFilterRule {
            id: format!("rule-{i}"),
            domain: None,
            title: Some(format!("re:pattern-{i}.*end")),
            cwd: None,
        });
    }
    config
}

fn bench_check_pane(c: &mut Criterion) {
    let mut group = c.benchmark_group("config/check_pane");

    // No rules — fast path
    let empty = PaneFilterConfig::default();
    group.bench_function("no_rules", |b| {
        b.iter(|| {
            empty.check_pane(
                black_box("local"),
                black_box("bash"),
                black_box("/home/user"),
            )
        });
    });

    // 5 exclude rules, no match (worst case for small configs)
    let small = make_filter_config(5);
    group.bench_function("5_rules_no_match", |b| {
        b.iter(|| {
            small.check_pane(
                black_box("local"),
                black_box("bash"),
                black_box("/home/user"),
            )
        });
    });

    // 5 exclude rules, first rule matches (best case)
    let small_match = make_filter_config(5);
    group.bench_function("5_rules_first_match", |b| {
        b.iter(|| {
            small_match.check_pane(
                black_box("local"),
                black_box("pattern-0 something end"),
                black_box("/home/user"),
            )
        });
    });

    // 50 exclude rules, no match (worst case for large configs)
    let large = make_filter_config(50);
    group.bench_function("50_rules_no_match", |b| {
        b.iter(|| {
            large.check_pane(
                black_box("local"),
                black_box("bash"),
                black_box("/home/user"),
            )
        });
    });

    // 50 exclude rules, last rule matches
    let large_last = make_filter_config(50);
    group.bench_function("50_rules_last_match", |b| {
        b.iter(|| {
            large_last.check_pane(
                black_box("local"),
                black_box("pattern-49 something end"),
                black_box("/home/user"),
            )
        });
    });

    group.finish();
}

// =============================================================================
// Benchmark: priority_for_pane
// =============================================================================

fn bench_priority_for_pane(c: &mut Criterion) {
    let mut group = c.benchmark_group("config/priority_for_pane");

    // No rules — returns default immediately
    let empty = PanePriorityConfig::default();
    group.bench_function("no_rules", |b| {
        b.iter(|| {
            empty.priority_for_pane(
                black_box("local"),
                black_box("bash"),
                black_box("/home/user"),
            )
        });
    });

    // Config with rules via TOML roundtrip
    let toml = r#"
[ingest.priorities]
default_priority = 5

[[ingest.priorities.rules]]
id = "high-vim"
title = "re:^vim\\b"
priority = 10

[[ingest.priorities.rules]]
id = "low-ssh"
domain = "SSH:*"
priority = 1
"#;
    let config = Config::from_toml(toml).unwrap();
    let prio = &config.ingest.priorities;

    group.bench_function("2_rules_no_match", |b| {
        b.iter(|| {
            prio.priority_for_pane(
                black_box("local"),
                black_box("bash"),
                black_box("/home/user"),
            )
        });
    });

    group.bench_function("2_rules_first_match", |b| {
        b.iter(|| {
            prio.priority_for_pane(
                black_box("local"),
                black_box("vim README.md"),
                black_box("/home/user"),
            )
        });
    });

    group.finish();
}

// =============================================================================
// Benchmark: Config round-trip (serialize + deserialize)
// =============================================================================

fn bench_config_roundtrip(c: &mut Criterion) {
    let config = Config::default();
    let toml_str = config.to_toml().unwrap();

    let mut group = c.benchmark_group("config/roundtrip");

    group.bench_function("serialize", |b| {
        b.iter(|| black_box(&config).to_toml().unwrap());
    });

    group.bench_function("deserialize", |b| {
        b.iter(|| Config::from_toml(black_box(&toml_str)).unwrap());
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_config_parse,
    bench_check_pane,
    bench_priority_for_pane,
    bench_config_roundtrip,
);
criterion_main!(benches);
