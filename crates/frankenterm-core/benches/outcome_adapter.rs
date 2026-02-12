//! Criterion benchmarks for the ft-specific Outcome adapter layer.
//!
//! Measures the overhead of `OutcomeExt::into_ft_result()`, `ResultExt::into_outcome()`,
//! `ft_outcome_to_result()`, and roundtrip conversions.
//!
//! Target: zero measurable overhead (same order of magnitude as bare Result ops).

use asupersync::{CancelKind, CancelReason, Outcome, PanicPayload, RegionId, Time};
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use frankenterm_core::Error;
use frankenterm_core::outcome::{
    FtOutcome, OutcomeExt, ResultExt, ft_outcome_to_result, ft_result_to_outcome,
};

fn test_cancel(kind: CancelKind) -> CancelReason {
    CancelReason {
        kind,
        origin_region: RegionId::new_ephemeral(),
        origin_task: None,
        timestamp: Time::ZERO,
        message: None,
        cause: None,
        truncated: false,
        truncated_at_depth: None,
    }
}

// ---------------------------------------------------------------------------
// into_ft_result benchmarks
// ---------------------------------------------------------------------------

fn bench_into_ft_result(c: &mut Criterion) {
    let mut group = c.benchmark_group("into_ft_result");

    group.bench_function("ok", |b| {
        b.iter(|| {
            let o = Outcome::<_, String>::ok(black_box(42));
            black_box(o.into_ft_result())
        });
    });

    group.bench_function("err", |b| {
        b.iter(|| {
            let o = Outcome::<i32, _>::err("fail".to_string());
            black_box(o.into_ft_result())
        });
    });

    group.bench_function("cancelled", |b| {
        b.iter(|| {
            let o = Outcome::<i32, String>::cancelled(test_cancel(CancelKind::Timeout));
            black_box(o.into_ft_result())
        });
    });

    group.bench_function("panicked", |b| {
        b.iter(|| {
            let o = Outcome::<i32, String>::panicked(PanicPayload::new("boom"));
            black_box(o.into_ft_result())
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// ft_outcome_to_result vs bare Result
// ---------------------------------------------------------------------------

fn bench_ft_outcome_to_result(c: &mut Criterion) {
    let mut group = c.benchmark_group("ft_outcome_to_result");

    group.bench_function("ok_baseline_result", |b| {
        b.iter(|| -> frankenterm_core::Result<i32> { black_box(Ok(42)) });
    });

    group.bench_function("ok_via_outcome", |b| {
        b.iter(|| {
            let o: FtOutcome<i32> = Outcome::ok(black_box(42));
            black_box(ft_outcome_to_result(o))
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// ResultExt::into_outcome
// ---------------------------------------------------------------------------

fn bench_result_ext(c: &mut Criterion) {
    let mut group = c.benchmark_group("result_ext");

    group.bench_function("into_outcome_ok", |b| {
        b.iter(|| {
            let r: frankenterm_core::Result<i32> = Ok(black_box(42));
            black_box(r.into_outcome())
        });
    });

    group.bench_function("into_outcome_err", |b| {
        b.iter(|| {
            let r: frankenterm_core::Result<i32> = Err(Error::Runtime("fail".into()));
            black_box(r.into_outcome())
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Roundtrip: Result → Outcome → ft_result
// ---------------------------------------------------------------------------

fn bench_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("roundtrip");

    group.bench_function("ok_result_outcome_result", |b| {
        b.iter(|| {
            let r: frankenterm_core::Result<i32> = Ok(black_box(42));
            let o = ft_result_to_outcome(r);
            black_box(ft_outcome_to_result(o))
        });
    });

    group.bench_function("err_result_outcome_result", |b| {
        b.iter(|| {
            let r: frankenterm_core::Result<i32> = Err(Error::Runtime("fail".into()));
            let o = ft_result_to_outcome(r);
            black_box(ft_outcome_to_result(o))
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Chaining 10 .map() on FtOutcome
// ---------------------------------------------------------------------------

fn bench_ft_chaining(c: &mut Criterion) {
    let mut group = c.benchmark_group("ft_chaining");

    group.bench_function("result_chain_10", |b| {
        b.iter(|| {
            let r: frankenterm_core::Result<i32> = Ok(black_box(1));
            let r = r.map(|x| x.wrapping_add(1));
            let r = r.map(|x| x.wrapping_mul(2));
            let r = r.map(|x| x.wrapping_add(3));
            let r = r.map(|x| x.wrapping_mul(4));
            let r = r.map(|x| x.wrapping_add(5));
            let r = r.map(|x| x.wrapping_mul(6));
            let r = r.map(|x| x.wrapping_add(7));
            let r = r.map(|x| x.wrapping_mul(8));
            let r = r.map(|x| x.wrapping_add(9));
            black_box(r.map(|x| x.wrapping_mul(10)))
        });
    });

    group.bench_function("ft_outcome_chain_10", |b| {
        b.iter(|| {
            let o: FtOutcome<i32> = Outcome::ok(black_box(1));
            let o = o.map(|x| x.wrapping_add(1));
            let o = o.map(|x| x.wrapping_mul(2));
            let o = o.map(|x| x.wrapping_add(3));
            let o = o.map(|x| x.wrapping_mul(4));
            let o = o.map(|x| x.wrapping_add(5));
            let o = o.map(|x| x.wrapping_mul(6));
            let o = o.map(|x| x.wrapping_add(7));
            let o = o.map(|x| x.wrapping_mul(8));
            let o = o.map(|x| x.wrapping_add(9));
            black_box(o.map(|x| x.wrapping_mul(10)))
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_into_ft_result,
    bench_ft_outcome_to_result,
    bench_result_ext,
    bench_roundtrip,
    bench_ft_chaining,
);
criterion_main!(benches);
