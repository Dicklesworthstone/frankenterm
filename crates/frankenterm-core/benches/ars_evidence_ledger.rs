//! Criterion coverage for ARS evidence ledger scaling.
//!
//! The workload builds synthetic workflow-forensics ledgers at 1k, 10k,
//! and 100k entries, then measures append, chain verification, and JSON
//! serialization latency.

use std::collections::BTreeMap;
use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core_ars::ars_evidence::{
    EvidenceCategory, EvidenceConfig, EvidenceLedger, EvidenceValue, EvidenceVerdict,
};

mod bench_common;

const ENTRY_SCALES: &[usize] = &[1_000, 10_000, 100_000];
const BASE_TS_US: u64 = 1_777_200_000_000_000;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "ars_evidence_ledger/append",
        budget: "append synthetic ARS evidence ledgers at 1k/10k/100k entries",
    },
    bench_common::BenchBudget {
        name: "ars_evidence_ledger/verify",
        budget: "verify hash-chain integrity for 1k/10k/100k entry ledgers",
    },
    bench_common::BenchBudget {
        name: "ars_evidence_ledger/serialize_json",
        budget: "serialize 1k/10k/100k entry ledgers to JSON bytes",
    },
];

fn ledger_config(entries: usize) -> EvidenceConfig {
    EvidenceConfig {
        min_entries: 1,
        max_entries: entries,
        hash_chain_enabled: true,
        required_categories: vec![
            EvidenceCategory::ChangeDetection,
            EvidenceCategory::SafetyProof,
            EvidenceCategory::SecretScan,
        ],
    }
}

fn category_for(index: usize) -> EvidenceCategory {
    match index % 7 {
        0 => EvidenceCategory::ChangeDetection,
        1 => EvidenceCategory::MdlExtraction,
        2 => EvidenceCategory::SafetyProof,
        3 => EvidenceCategory::SecretScan,
        4 => EvidenceCategory::ParameterBounds,
        5 => EvidenceCategory::TimeoutCalc,
        _ => EvidenceCategory::ContextSnapshot,
    }
}

fn verdict_for(index: usize) -> EvidenceVerdict {
    if index % 997 == 0 {
        EvidenceVerdict::Neutral
    } else {
        EvidenceVerdict::Support
    }
}

fn payload_for(index: usize) -> BTreeMap<String, EvidenceValue> {
    let mut payload = BTreeMap::new();
    payload.insert(
        "workflow_id".to_string(),
        EvidenceValue::String(format!("workflow-{index:06}")),
    );
    payload.insert(
        "pane_id".to_string(),
        EvidenceValue::Number((index % 200) as f64),
    );
    payload.insert(
        "confidence".to_string(),
        EvidenceValue::Number(0.80 + ((index % 20) as f64 / 100.0)),
    );
    payload.insert(
        "risk_score".to_string(),
        EvidenceValue::Number((index % 37) as f64 / 37.0),
    );
    payload.insert("approved".to_string(), EvidenceValue::Bool(index % 13 != 0));
    payload.insert(
        "signals".to_string(),
        EvidenceValue::StringList(vec![
            format!("rule-{}", index % 17),
            format!("stage-{}", index % 5),
            format!("span-{}", index % 11),
        ]),
    );
    payload
}

fn append_entries(ledger: &mut EvidenceLedger, entries: usize) {
    for index in 0..entries {
        let appended = ledger.append(
            category_for(index),
            BASE_TS_US + index as u64,
            format!(
                "ARS workflow evidence entry {index}: category={} pane={} stage={}",
                category_for(index),
                index % 200,
                index % 5
            ),
            payload_for(index),
            verdict_for(index),
        );
        assert!(
            appended,
            "synthetic ledger max_entries should admit entry {index}"
        );
    }
}

fn build_ledger(entries: usize) -> EvidenceLedger {
    let mut ledger = EvidenceLedger::new(ledger_config(entries));
    append_entries(&mut ledger, entries);
    assert_eq!(ledger.len(), entries);
    ledger
}

fn bench_ars_evidence_ledger(c: &mut Criterion) {
    bench_common::emit_bench_artifacts("ars_evidence_ledger", BUDGETS);

    let mut group = c.benchmark_group("ars_evidence_ledger");
    group.sample_size(10);

    for &entries in ENTRY_SCALES {
        group.throughput(Throughput::Elements(entries as u64));
        group.bench_with_input(
            BenchmarkId::new("append", entries),
            &entries,
            |b, &entries| {
                b.iter_batched(
                    || EvidenceLedger::new(ledger_config(entries)),
                    |mut ledger| {
                        append_entries(&mut ledger, entries);
                        black_box(ledger.len())
                    },
                    BatchSize::SmallInput,
                );
            },
        );

        let ledger = build_ledger(entries);
        group.bench_with_input(BenchmarkId::new("verify", entries), &ledger, |b, ledger| {
            b.iter(|| {
                let verification = black_box(ledger).verify_chain();
                assert!(verification.is_valid);
                black_box(verification.entries_checked)
            });
        });

        group.bench_with_input(
            BenchmarkId::new("serialize_json", entries),
            &ledger,
            |b, ledger| {
                b.iter(|| {
                    let bytes = serde_json::to_vec(black_box(ledger)).expect("serialize ledger");
                    black_box(bytes.len())
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_ars_evidence_ledger);
criterion_main!(benches);
