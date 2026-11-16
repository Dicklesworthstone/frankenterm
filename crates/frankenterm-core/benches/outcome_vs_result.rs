//! Criterion benchmarks: Outcome<T,E> vs Result<T,E> overhead.
//!
//! Verifies that Outcome operations have negligible overhead compared
//! to equivalent Result operations.
//!
//! Performance budget:
//! - Construction: identical or < 5ns overhead
//! - Pattern matching: identical codegen
//! - Chaining 10 operations: < 10% overhead vs Result
//! - Error propagation: < 10% overhead vs Result

use asupersync::{CancelKind, CancelReason, Outcome, PanicPayload, RegionId, Time};
use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

// ---------------------------------------------------------------------------
// Construction benchmarks
// ---------------------------------------------------------------------------

fn bench_construction(c: &mut Criterion) {
    let mut group = c.benchmark_group("construction");

    group.bench_function("result_ok", |b| {
        b.iter(|| -> Result<i32, String> { black_box(Ok(42)) });
    });

    group.bench_function("outcome_ok", |b| {
        b.iter(|| -> Outcome<i32, String> { black_box(Outcome::ok(42)) });
    });

    group.bench_function("result_err", |b| {
        b.iter(|| -> Result<i32, String> { black_box(Err("error".to_string())) });
    });

    group.bench_function("outcome_err", |b| {
        b.iter(|| -> Outcome<i32, String> { black_box(Outcome::err("error".to_string())) });
    });

    group.bench_function("outcome_cancelled", |b| {
        b.iter(|| -> Outcome<i32, String> {
            black_box(Outcome::cancelled(CancelReason {
                kind: CancelKind::User,
                origin_region: RegionId::new_ephemeral(),
                origin_task: None,
                timestamp: Time::ZERO,
                message: None,
                cause: None,
                truncated: false,
                truncated_at_depth: None,
            }))
        });
    });

    group.bench_function("outcome_panicked", |b| {
        b.iter(|| -> Outcome<i32, String> {
            black_box(Outcome::panicked(PanicPayload::new("boom")))
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Pattern matching benchmarks
// ---------------------------------------------------------------------------

fn bench_pattern_matching(c: &mut Criterion) {
    let mut group = c.benchmark_group("pattern_matching");

    let result_ok: Result<i32, String> = Ok(42);
    let outcome_ok: Outcome<i32, String> = Outcome::ok(42);

    group.bench_function("result_match_ok", |b| {
        b.iter(|| match black_box(&result_ok) {
            Ok(v) => black_box(*v),
            Err(_) => 0,
        });
    });

    group.bench_function("outcome_match_ok", |b| {
        b.iter(|| match black_box(&outcome_ok) {
            Outcome::Ok(v) => black_box(*v),
            Outcome::Err(_) => 0,
            Outcome::Cancelled(_) => -1,
            Outcome::Panicked(_) => -2,
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Chaining 10 operations benchmark
// ---------------------------------------------------------------------------

fn chain_result(v: i32) -> Result<i32, String> {
    let r = Ok(v);
    let r = r.map(|x| x.wrapping_add(1));
    let r = r.map(|x| x.wrapping_mul(2));
    let r = r.map(|x| x.wrapping_add(3));
    let r = r.map(|x| x.wrapping_mul(4));
    let r = r.map(|x| x.wrapping_add(5));
    let r = r.map(|x| x.wrapping_mul(6));
    let r = r.map(|x| x.wrapping_add(7));
    let r = r.map(|x| x.wrapping_mul(8));
    let r = r.map(|x| x.wrapping_add(9));
    r.map(|x| x.wrapping_mul(10))
}

fn chain_outcome(v: i32) -> Outcome<i32, String> {
    let o = Outcome::ok(v);
    let o = o.map(|x| x.wrapping_add(1));
    let o = o.map(|x| x.wrapping_mul(2));
    let o = o.map(|x| x.wrapping_add(3));
    let o = o.map(|x| x.wrapping_mul(4));
    let o = o.map(|x| x.wrapping_add(5));
    let o = o.map(|x| x.wrapping_mul(6));
    let o = o.map(|x| x.wrapping_add(7));
    let o = o.map(|x| x.wrapping_mul(8));
    let o = o.map(|x| x.wrapping_add(9));
    o.map(|x| x.wrapping_mul(10))
}

fn bench_chaining(c: &mut Criterion) {
    let mut group = c.benchmark_group("chaining_10_ops");

    group.bench_function("result_chain", |b| {
        b.iter(|| chain_result(black_box(1)));
    });

    group.bench_function("outcome_chain", |b| {
        b.iter(|| chain_outcome(black_box(1)));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Error propagation benchmark (simulating ? operator)
// ---------------------------------------------------------------------------

fn propagate_result_ok(depth: i32) -> Result<i32, String> {
    if depth <= 0 {
        return Ok(depth);
    }
    let v = propagate_result_ok(depth - 1)?;
    Ok(v.wrapping_add(1))
}

fn propagate_outcome_ok(depth: i32) -> Outcome<i32, String> {
    if depth <= 0 {
        return Outcome::ok(depth);
    }
    let v = match propagate_outcome_ok(depth - 1) {
        Outcome::Ok(v) => v,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    Outcome::ok(v.wrapping_add(1))
}

fn bench_propagation(c: &mut Criterion) {
    let mut group = c.benchmark_group("error_propagation");

    group.bench_function("result_propagate_10", |b| {
        b.iter(|| propagate_result_ok(black_box(10)));
    });

    group.bench_function("outcome_propagate_10", |b| {
        b.iter(|| propagate_outcome_ok(black_box(10)));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Conversion overhead benchmark
// ---------------------------------------------------------------------------

fn bench_conversion(c: &mut Criterion) {
    let mut group = c.benchmark_group("conversion");

    group.bench_function("result_to_outcome", |b| {
        b.iter(|| {
            let r: Result<i32, String> = Ok(black_box(42));
            let _o: Outcome<i32, String> = Outcome::from(r);
        });
    });

    group.bench_function("outcome_into_result", |b| {
        b.iter(|| {
            let o: Outcome<i32, String> = Outcome::ok(black_box(42));
            let _r = o.into_result();
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Group and entry point
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_construction,
    bench_pattern_matching,
    bench_chaining,
    bench_propagation,
    bench_conversion,
);
criterion_main!(benches);
