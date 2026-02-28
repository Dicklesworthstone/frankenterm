//! Benchmarks for reactive dataflow graph propagation.
//!
//! Performance targets from bead `ft-283h4.5`:
//! - `bench_single_node_propagation`: < 1us
//! - `bench_chain_propagation_10`: < 10us
//! - `bench_fanout_50`: < 50us
//! - `bench_200_node_graph`: < 200us
//! - `bench_concurrent_updates`: < 100us total

use criterion::{criterion_group, criterion_main, Criterion};
use frankenterm_core::dataflow::{DataflowGraph, NodeId, Value};
use std::hint::black_box;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "dataflow_bench/single_node_propagation",
        budget: "target <1us per source update through one map node",
    },
    bench_common::BenchBudget {
        name: "dataflow_bench/chain_propagation_10",
        budget: "target <10us for 10-node map chain update",
    },
    bench_common::BenchBudget {
        name: "dataflow_bench/fanout_50",
        budget: "target <50us for one source fanout to 50 combine nodes",
    },
    bench_common::BenchBudget {
        name: "dataflow_bench/graph_200_nodes",
        budget: "target <200us for realistic 200-node graph update",
    },
    bench_common::BenchBudget {
        name: "dataflow_bench/concurrent_updates_10_sources",
        budget: "target <100us total for batched updates across 10 sources",
    },
];

fn build_single_node_graph() -> (DataflowGraph, NodeId, NodeId) {
    let mut graph = DataflowGraph::new();
    let source = graph.add_source("source", Value::Int(0));
    let mapped = graph.add_map("mapped", vec![source], |inputs| inputs[0].clone());
    graph.propagate();
    (graph, source, mapped)
}

fn build_chain_graph(length: usize) -> (DataflowGraph, NodeId, NodeId) {
    let mut graph = DataflowGraph::new();
    let source = graph.add_source("source", Value::Int(0));
    let mut tail = source;
    for idx in 0..length {
        tail = graph.add_map(
            &format!("chain_{idx}"),
            vec![tail],
            |inputs| match &inputs[0] {
                Value::Int(value) => Value::Int(value.saturating_add(1)),
                _ => Value::None,
            },
        );
    }
    graph.propagate();
    (graph, source, tail)
}

fn build_fanout_graph(width: usize) -> (DataflowGraph, NodeId, Vec<NodeId>) {
    let mut graph = DataflowGraph::new();
    let source = graph.add_source("source", Value::Int(0));
    let constant = graph.add_source("constant", Value::Int(1));
    let mut terminals = Vec::with_capacity(width);
    for idx in 0..width {
        let node =
            graph.add_combine(
                &format!("fanout_{idx}"),
                vec![source, constant],
                |inputs| match (&inputs[0], &inputs[1]) {
                    (Value::Int(left), Value::Int(right)) => {
                        Value::Int(left.saturating_add(*right))
                    }
                    _ => Value::None,
                },
            );
        terminals.push(node);
    }
    graph.propagate();
    (graph, source, terminals)
}

fn build_200_node_graph() -> (DataflowGraph, NodeId, Vec<NodeId>) {
    let mut graph = DataflowGraph::new();
    let mut sources = Vec::with_capacity(20);
    for idx in 0..20 {
        sources.push(graph.add_source(&format!("source_{idx}"), Value::Int(0)));
    }

    let mut maps = Vec::with_capacity(100);
    for idx in 0..100 {
        let upstream = sources[idx % sources.len()];
        maps.push(graph.add_map(
            &format!("map_{idx}"),
            vec![upstream],
            |inputs| match &inputs[0] {
                Value::Int(value) => Value::Int(value.saturating_add(1)),
                _ => Value::None,
            },
        ));
    }

    let mut combines = Vec::with_capacity(80);
    for idx in 0..80 {
        let left = maps[idx % maps.len()];
        let right = maps[(idx * 7 + 3) % maps.len()];
        combines.push(
            graph.add_combine(
                &format!("combine_{idx}"),
                vec![left, right],
                |inputs| match (&inputs[0], &inputs[1]) {
                    (Value::Int(a), Value::Int(b)) => Value::Int(a.saturating_add(*b)),
                    _ => Value::None,
                },
            ),
        );
    }

    graph.propagate();
    assert_eq!(graph.node_count(), 200);
    (graph, sources[0], combines)
}

fn build_concurrent_update_graph() -> (DataflowGraph, Vec<NodeId>, NodeId) {
    let mut graph = DataflowGraph::new();
    let mut sources = Vec::with_capacity(10);
    let mut derived = Vec::with_capacity(10);

    for idx in 0..10 {
        let source = graph.add_source(&format!("source_{idx}"), Value::Int(0));
        let mapped = graph.add_map(&format!("mapped_{idx}"), vec![source], |inputs| {
            inputs[0].clone()
        });
        sources.push(source);
        derived.push(mapped);
    }

    let aggregate = graph.add_combine("aggregate", derived, |inputs| {
        let total = inputs.iter().fold(0_i64, |acc, value| match value {
            Value::Int(v) => acc.saturating_add(*v),
            _ => acc,
        });
        Value::Int(total)
    });

    graph.propagate();
    (graph, sources, aggregate)
}

fn bench_single_node_propagation(c: &mut Criterion) {
    let (mut graph, source, mapped) = build_single_node_graph();
    let mut tick = 0_i64;

    c.bench_function("bench_single_node_propagation", |b| {
        b.iter(|| {
            tick = tick.wrapping_add(1);
            graph
                .update_source(source, Value::Int(tick))
                .expect("source update should succeed");
            let stats = graph.propagate();
            black_box(stats);
            black_box(graph.get_value(mapped));
        });
    });
}

fn bench_chain_propagation_10(c: &mut Criterion) {
    let (mut graph, source, tail) = build_chain_graph(10);
    let mut tick = 0_i64;

    c.bench_function("bench_chain_propagation_10", |b| {
        b.iter(|| {
            tick = tick.wrapping_add(1);
            graph
                .update_source(source, Value::Int(tick))
                .expect("source update should succeed");
            let stats = graph.propagate();
            black_box(stats);
            black_box(graph.get_value(tail));
        });
    });
}

fn bench_fanout_50(c: &mut Criterion) {
    let (mut graph, source, terminals) = build_fanout_graph(50);
    let mut tick = 0_i64;

    c.bench_function("bench_fanout_50", |b| {
        b.iter(|| {
            tick = tick.wrapping_add(1);
            graph
                .update_source(source, Value::Int(tick))
                .expect("source update should succeed");
            let stats = graph.propagate();
            black_box(stats);
            black_box(graph.get_value(terminals[0]));
            black_box(graph.get_value(terminals[terminals.len() - 1]));
        });
    });
}

fn bench_200_node_graph(c: &mut Criterion) {
    let (mut graph, source, terminals) = build_200_node_graph();
    let mut tick = 0_i64;

    c.bench_function("bench_200_node_graph", |b| {
        b.iter(|| {
            tick = tick.wrapping_add(1);
            graph
                .update_source(source, Value::Int(tick))
                .expect("source update should succeed");
            let stats = graph.propagate();
            black_box(stats);
            black_box(graph.get_value(terminals[0]));
            black_box(graph.get_value(terminals[terminals.len() - 1]));
        });
    });
}

fn bench_concurrent_updates(c: &mut Criterion) {
    let (mut graph, sources, aggregate) = build_concurrent_update_graph();
    let mut tick = 0_i64;

    c.bench_function("bench_concurrent_updates", |b| {
        b.iter(|| {
            tick = tick.wrapping_add(1);
            for (offset, source) in sources.iter().enumerate() {
                let next_value = tick.saturating_add(offset as i64);
                graph
                    .update_source(*source, Value::Int(next_value))
                    .expect("source update should succeed");
            }
            let stats = graph.propagate();
            black_box(stats);
            black_box(graph.get_value(aggregate));
        });
    });
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("dataflow_bench", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_single_node_propagation,
        bench_chain_propagation_10,
        bench_fanout_50,
        bench_200_node_graph,
        bench_concurrent_updates
);
criterion_main!(benches);
