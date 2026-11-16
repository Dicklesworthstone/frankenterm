//! Benchmarks for `Cx` propagation through layered call chains.
//!
//! This targets the `wa-2lp7o` requirement to quantify context-threading
//! overhead at representative call depths.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use frankenterm_core::cx::{Cx, for_testing, with_cx};

mod bench_common;

const DEPTHS: [usize; 4] = [1, 5, 10, 20];
const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "cx_propagation/depth_1",
        budget: "context pass-through overhead at depth 1",
    },
    bench_common::BenchBudget {
        name: "cx_propagation/depth_5",
        budget: "context pass-through overhead at depth 5",
    },
    bench_common::BenchBudget {
        name: "cx_propagation/depth_10",
        budget: "context pass-through overhead at depth 10",
    },
    bench_common::BenchBudget {
        name: "cx_propagation/depth_20",
        budget: "context pass-through overhead at depth 20",
    },
];

fn propagate(depth: usize, cx: &Cx) -> usize {
    if depth == 0 {
        return 0;
    }

    with_cx(cx, |inner| 1 + propagate(depth - 1, inner))
}

fn bench_cx_propagation(c: &mut Criterion) {
    let mut group = c.benchmark_group("cx_propagation");
    let cx = for_testing();

    for depth in DEPTHS {
        group.bench_with_input(BenchmarkId::new("depth", depth), &depth, |b, &depth| {
            b.iter(|| {
                let threaded = propagate(black_box(depth), &cx);
                black_box(threaded);
            });
        });
    }

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("cx_propagation", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_cx_propagation
);
criterion_main!(benches);
