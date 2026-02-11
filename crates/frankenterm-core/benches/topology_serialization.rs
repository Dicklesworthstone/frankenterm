//! Benchmarks for session topology serialization.
//!
//! Performance budgets:
//! - from_panes (10 panes): **< 100µs**
//! - from_panes (50 panes): **< 500µs**
//! - JSON roundtrip: **< 50µs** per snapshot
//! - Pane matching: **< 200µs** for 50-pane topology

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::session_topology::{PaneNode, TopologySnapshot};
use frankenterm_core::wezterm::PaneInfo;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "from_panes_small",
        budget: "p50 < 100µs (10-pane topology)",
    },
    bench_common::BenchBudget {
        name: "from_panes_medium",
        budget: "p50 < 500µs (50-pane topology)",
    },
    bench_common::BenchBudget {
        name: "json_roundtrip",
        budget: "p50 < 50µs per snapshot",
    },
    bench_common::BenchBudget {
        name: "pane_matching",
        budget: "p50 < 200µs (50-pane matching)",
    },
];

/// Generate mock PaneInfo entries simulating a multi-window layout.
fn generate_panes(count: usize) -> Vec<PaneInfo> {
    let mut panes = Vec::with_capacity(count);
    let panes_per_tab = 4; // 2x2 grid per tab
    let tabs_per_window = 3;

    for i in 0..count {
        let window_id = (i / (panes_per_tab * tabs_per_window)) as u64;
        let tab_id = (i / panes_per_tab) as u64;
        let pane_in_tab = i % panes_per_tab;

        // Simulate 2x2 grid positions
        let (left, top) = match pane_in_tab {
            0 => (0, 0),
            1 => (80, 0),
            2 => (0, 24),
            3 => (80, 24),
            _ => (0, 0),
        };

        let mut extra = serde_json::Map::new();
        extra.insert("workspace".to_string(), serde_json::json!("default"));

        panes.push(PaneInfo {
            window_id,
            tab_id,
            pane_id: i as u64,
            title: format!("pane-{i}"),
            cwd: format!("file:///home/user/project-{i}"),
            cursor_x: 0,
            cursor_y: 0,
            cursor_visibility: "visible".to_string(),
            left,
            top,
            width: 80,
            height: 24,
            pixel_width: 640,
            pixel_height: 384,
            is_active: i == 0,
            is_zoomed: false,
            tab_is_active: true,
            extra: serde_json::Value::Object(extra),
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

        group.bench_with_input(
            BenchmarkId::new("deserialize", count),
            &json,
            |b, json| {
                b.iter(|| TopologySnapshot::from_json(json).unwrap());
            },
        );
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
                b.iter(|| {
                    frankenterm_core::session_topology::match_panes(old, new)
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_from_panes,
    bench_json_roundtrip,
    bench_pane_count,
    bench_pane_ids,
    bench_pane_matching,
);
criterion_main!(benches);
