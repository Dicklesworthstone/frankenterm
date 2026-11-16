//! Criterion benchmarks for quota-aware account selection paths.
//!
//! Focuses on the hot lookup path used by scheduling/launch decisions:
//! `select_account` + `build_quota_advisory`.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::accounts::{
    AccountRecord, AccountSelectionConfig, DEFAULT_LOW_QUOTA_THRESHOLD_PERCENT,
    build_quota_advisory, select_account,
};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "caut_quota_scheduling/quota_lookup_warm_cache",
        budget: "Warm-path quota lookup (select_account + advisory) should stay sub-10ms",
    },
    bench_common::BenchBudget {
        name: "caut_quota_scheduling/quota_lookup_cold_path",
        budget: "Cold-path quota lookup with per-iter account materialization remains bounded",
    },
];

fn synthetic_accounts(count: usize) -> Vec<AccountRecord> {
    let mut accounts = Vec::with_capacity(count);
    for i in 0..count {
        let now = 1_700_000_000_000_i64 + i64::try_from(i).unwrap_or(0);
        let i_u64 = u64::try_from(i).unwrap_or(0);
        let bucket = (i_u64.wrapping_mul(73)) % 10_000;
        let percent_remaining = (bucket as f64) / 100.0;
        let last_used_at = if i % 4 == 0 {
            None
        } else {
            Some(now.saturating_sub(i64::try_from(i).unwrap_or(0) * 50))
        };

        accounts.push(AccountRecord {
            id: i64::try_from(i).unwrap_or(0),
            account_id: format!("acct-{i:03}"),
            service: "openai".to_string(),
            name: Some(format!("Account {i}")),
            percent_remaining,
            reset_at: None,
            tokens_used: Some(10_000 + i64::try_from(i).unwrap_or(0)),
            tokens_remaining: Some(5_000 - i64::try_from(i % 500).unwrap_or(0)),
            tokens_limit: Some(15_000),
            last_refreshed_at: now,
            last_used_at,
            created_at: now,
            updated_at: now,
        });
    }
    accounts
}

fn bench_quota_lookup_warm_cache(c: &mut Criterion) {
    let mut group = c.benchmark_group("caut_quota_scheduling/quota_lookup_warm_cache");
    let config = AccountSelectionConfig {
        threshold_percent: 5.0,
    };

    for &accounts_count in &[1usize, 5, 16, 64, 200] {
        let accounts = synthetic_accounts(accounts_count);
        group.throughput(Throughput::Elements(accounts_count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(accounts_count),
            &accounts,
            |b, accounts| {
                b.iter(|| {
                    let selection =
                        select_account(black_box(accounts.as_slice()), black_box(&config));
                    let advisory = build_quota_advisory(
                        black_box(&selection),
                        DEFAULT_LOW_QUOTA_THRESHOLD_PERCENT,
                    );
                    black_box(advisory.is_blocking());
                });
            },
        );
    }

    group.finish();
}

fn bench_quota_lookup_cold_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("caut_quota_scheduling/quota_lookup_cold_path");
    let config = AccountSelectionConfig {
        threshold_percent: 5.0,
    };

    for &accounts_count in &[1usize, 5, 16, 64] {
        group.throughput(Throughput::Elements(accounts_count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(accounts_count),
            &accounts_count,
            |b, &accounts_count| {
                b.iter(|| {
                    let accounts = synthetic_accounts(accounts_count);
                    let selection =
                        select_account(black_box(accounts.as_slice()), black_box(&config));
                    let advisory = build_quota_advisory(
                        black_box(&selection),
                        DEFAULT_LOW_QUOTA_THRESHOLD_PERCENT,
                    );
                    black_box(advisory.warning.as_deref());
                });
            },
        );
    }

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("caut_quota_scheduling", BUDGETS);
    Criterion::default()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_quota_lookup_warm_cache, bench_quota_lookup_cold_path
);
criterion_main!(benches);
