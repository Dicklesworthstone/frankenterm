//! Benchmarks for sharded mux routing and assignment performance.
//!
//! Performance budgets:
//! - routing overhead: **< 50us** per `get_text` call versus direct backend
//! - list aggregation: scales near-linearly across 2/4/8 shards
//! - assignment throughput: stable under 100/500/1000-pane batches

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::patterns::AgentType;
use frankenterm_core::sharding::{
    AssignmentStrategy, ShardBackend, ShardId, ShardedWeztermClient, assign_pane_with_strategy,
};
use frankenterm_core::wezterm::{MockWezterm, WeztermHandle};
use std::hint::black_box;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "cross_shard_routing_overhead/direct_vs_sharded_get_text",
        budget: "routing overhead < 50us per call",
    },
    bench_common::BenchBudget {
        name: "list_all_panes_aggregation/list_panes",
        budget: "aggregation overhead scales near-linearly across shard count",
    },
    bench_common::BenchBudget {
        name: "shard_assignment_throughput/{round_robin,by_agent_type,manual}",
        budget: "stable throughput across 100/500/1000 pane batches",
    },
];

const AGENT_CYCLE: [AgentType; 4] = [
    AgentType::Codex,
    AgentType::ClaudeCode,
    AgentType::Gemini,
    AgentType::Unknown,
];

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime for benchmark")
}

fn build_sharded_handle(
    rt: &tokio::runtime::Runtime,
    shard_count: usize,
    panes_per_shard: usize,
) -> (WeztermHandle, u64, u64) {
    let mut backends = Vec::with_capacity(shard_count);

    for shard_idx in 0..shard_count {
        let backend = Arc::new(MockWezterm::new());
        rt.block_on(async {
            for local_idx in 1..=panes_per_shard as u64 {
                backend.add_default_pane(local_idx).await;
                backend
                    .inject_output(local_idx, "sharding benchmark payload")
                    .await
                    .expect("inject output");
            }
        });

        let handle: WeztermHandle = backend;
        backends.push(ShardBackend::new(
            ShardId(shard_idx),
            format!("shard-{shard_idx}"),
            handle,
        ));
    }

    let client = ShardedWeztermClient::new(backends, AssignmentStrategy::RoundRobin)
        .expect("create sharded client");
    let handle: WeztermHandle = Arc::new(client);

    let probe_local_id = 1_u64;
    let probe_shard_id = if shard_count > 1 {
        ShardId(1)
    } else {
        ShardId(0)
    };
    let probe_global_id = frankenterm_core::sharding::encode_sharded_pane_id(probe_shard_id, 1);

    rt.block_on(async {
        let panes = handle.list_panes().await.expect("list panes");
        let total_expected = shard_count * panes_per_shard;
        assert_eq!(panes.len(), total_expected);
    });

    (handle, probe_local_id, probe_global_id)
}

fn bench_cross_shard_routing_overhead(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("cross_shard_routing_overhead");
    group.sample_size(120);
    group.measurement_time(Duration::from_secs(12));

    let direct_backend = Arc::new(MockWezterm::new());
    rt.block_on(async {
        direct_backend.add_default_pane(1).await;
        direct_backend
            .inject_output(1, "sharding benchmark payload")
            .await
            .expect("inject direct output");
    });
    let direct: WeztermHandle = direct_backend;

    for &shards in &[2_usize, 4, 8] {
        let (sharded, direct_probe_id, global_probe_id) = build_sharded_handle(&rt, shards, 8);

        group.bench_with_input(
            BenchmarkId::new("direct_get_text", shards),
            &direct_probe_id,
            |b, &pane_id| {
                b.to_async(&rt).iter(|| async {
                    let text = direct
                        .get_text(pane_id, false)
                        .await
                        .expect("direct get_text");
                    black_box(text.len());
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("sharded_get_text", shards),
            &global_probe_id,
            |b, &pane_id| {
                b.to_async(&rt).iter(|| async {
                    let text = sharded
                        .get_text(pane_id, false)
                        .await
                        .expect("sharded get_text");
                    black_box(text.len());
                });
            },
        );
    }

    group.finish();
}

fn bench_list_all_panes_aggregation(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("list_all_panes_aggregation");
    group.sample_size(80);
    group.measurement_time(Duration::from_secs(10));

    for &shards in &[2_usize, 4, 8] {
        let panes_per_shard = 32;
        let (handle, _, _) = build_sharded_handle(&rt, shards, panes_per_shard);
        group.throughput(Throughput::Elements((shards * panes_per_shard) as u64));
        group.bench_with_input(BenchmarkId::new("list_panes", shards), &shards, |b, _| {
            b.to_async(&rt).iter(|| async {
                let panes = handle.list_panes().await.expect("list panes");
                black_box(panes.len());
            });
        });
    }

    group.finish();
}

fn bench_shard_assignment_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("shard_assignment_throughput");
    let shards: Vec<ShardId> = (0..8).map(ShardId).collect();
    let by_agent_strategy = AssignmentStrategy::ByAgentType {
        agent_to_shard: HashMap::from([
            (AgentType::Codex, ShardId(0)),
            (AgentType::ClaudeCode, ShardId(1)),
            (AgentType::Gemini, ShardId(2)),
            (AgentType::Unknown, ShardId(3)),
        ]),
        default_shard: Some(ShardId(0)),
    };
    let round_robin_strategy = AssignmentStrategy::RoundRobin;

    for &pane_count in &[100_usize, 500, 1000] {
        group.throughput(Throughput::Elements(pane_count as u64));
        let pane_ids: Vec<u64> = (1..=pane_count as u64).collect();
        let manual_strategy = AssignmentStrategy::Manual {
            pane_to_shard: pane_ids
                .iter()
                .map(|pane_id| (*pane_id, shards[*pane_id as usize % shards.len()]))
                .collect(),
            default_shard: Some(ShardId(0)),
        };

        group.bench_with_input(
            BenchmarkId::new("round_robin", pane_count),
            &pane_ids,
            |b, pane_ids| {
                b.iter(|| {
                    let mut checksum = 0usize;
                    for (idx, pane_id) in pane_ids.iter().enumerate() {
                        let shard = assign_pane_with_strategy(
                            &round_robin_strategy,
                            &shards,
                            *pane_id,
                            None,
                            Some(AGENT_CYCLE[idx % AGENT_CYCLE.len()]),
                        );
                        checksum ^= shard.0;
                    }
                    black_box(checksum);
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("by_agent_type", pane_count),
            &pane_ids,
            |b, pane_ids| {
                b.iter(|| {
                    let mut checksum = 0usize;
                    for (idx, pane_id) in pane_ids.iter().enumerate() {
                        let shard = assign_pane_with_strategy(
                            &by_agent_strategy,
                            &shards,
                            *pane_id,
                            None,
                            Some(AGENT_CYCLE[idx % AGENT_CYCLE.len()]),
                        );
                        checksum ^= shard.0;
                    }
                    black_box(checksum);
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("manual", pane_count),
            &pane_ids,
            |b, pane_ids| {
                b.iter(|| {
                    let mut checksum = 0usize;
                    for pane_id in pane_ids {
                        let shard = assign_pane_with_strategy(
                            &manual_strategy,
                            &shards,
                            *pane_id,
                            None,
                            None,
                        );
                        checksum ^= shard.0;
                    }
                    black_box(checksum);
                });
            },
        );
    }

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("shard_routing", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_cross_shard_routing_overhead,
        bench_list_all_panes_aggregation,
        bench_shard_assignment_throughput
);
criterion_main!(benches);
