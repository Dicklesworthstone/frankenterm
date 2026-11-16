//! Benchmarks for `Cx` creation overhead under asupersync.
//!
//! Performance budgets (from wa-hj458):
//! - Cx instantiation: **< 50ns** per Cx instantiation.

use asupersync::runtime::{RuntimeBuilder, RuntimeHandle};
use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "cx_creation/current_thread",
        budget: "< 50ns per Cx instantiation",
    },
    bench_common::BenchBudget {
        name: "cx_creation/multi_thread",
        budget: "< 50ns per Cx instantiation",
    },
];

#[derive(Clone)]
struct Cx {
    runtime: RuntimeHandle,
}

impl Cx {
    fn from_handle(handle: &RuntimeHandle) -> Self {
        Self {
            runtime: handle.clone(),
        }
    }
}

fn bench_cx_creation(c: &mut Criterion) {
    let mut group = c.benchmark_group("cx_creation");

    let rt_current = RuntimeBuilder::current_thread()
        .build()
        .expect("build current_thread runtime");
    let handle_current = rt_current.handle();
    group.bench_function("current_thread", |b| {
        b.iter(|| {
            let cx = Cx::from_handle(&handle_current);
            black_box(&cx.runtime);
        });
    });

    let rt_multi = RuntimeBuilder::multi_thread()
        .build()
        .expect("build multi_thread runtime");
    let handle_multi = rt_multi.handle();
    group.bench_function("multi_thread", |b| {
        b.iter(|| {
            let cx = Cx::from_handle(&handle_multi);
            black_box(&cx.runtime);
        });
    });

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("cx_creation", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_cx_creation
);
criterion_main!(benches);
