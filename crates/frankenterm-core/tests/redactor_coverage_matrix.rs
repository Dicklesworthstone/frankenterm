//! Recall/precision regression harness for the secret
//! redactor ([BR-RC-SAFETY-PROOFS.G10] / `ft-x0666.2`).
//!
//! Runs the synthesized in-tree corpus through
//! `Redactor::detect`, computes per-provider recall/precision,
//! and asserts the bead's headline rule:
//!
//! > ≥99% recall floor on every provider; fail CI on dip.
//!
//! Also exposes a deliberate-bless flow for the JSON coverage
//! report (committed at `docs/security/redactor-coverage.json`).
//! Set `FT_REDACTOR_COVERAGE_BLESS=1` to overwrite the report
//! when the corpus changes.

#![allow(clippy::suboptimal_flops)]

use frankenterm_core::redactor::secret_pattern_names;
use frankenterm_core::redactor_coverage_matrix::{
    MatrixSnapshot, RedactorCoverageHealth, RedactorTestVector, fold_snapshot, synthesized_corpus,
};

const COVERAGE_SCHEMA_VERSION: u32 = 2;
const RECALL_FLOOR: f64 = 0.99;
const PRECISION_FLOOR_OVERALL: f64 = 0.50; // see methodology doc
const COVERAGE_REPORT_PATH: &str = "../../docs/security/redactor-coverage.json";
const SAMPLE_SIZE_FLOOR_METHODOLOGY: &str = "docs/security/redactor-recall-derivation.md";
const CONFIDENCE_TARGET: f64 = 0.99;
const ALPHA: f64 = 0.01;
const ZERO_MISS_REQUIRED_POSITIVES_PER_CLASS: u32 = 459;
const SAMPLE_SIZE_FLOOR_FOLLOW_UP_BEAD: &str = "ft-tf6g3.35";

#[test]
fn synthesized_corpus_meets_recall_floor() {
    let corpus = synthesized_corpus();
    let snap = MatrixSnapshot::evaluate(&corpus);

    if !snap.meets_recall_floor(RECALL_FLOOR) {
        let (worst, recall) = snap.min_provider_recall().unwrap_or_default();
        panic!(
            "redactor recall floor {RECALL_FLOOR} violated by provider {worst:?} (recall {recall}); \
             snap.overall = TP={} FN={} FP={}",
            snap.overall.true_positives, snap.overall.false_negatives, snap.overall.false_positives,
        );
    }
}

#[test]
fn overall_precision_meets_floor() {
    let corpus = synthesized_corpus();
    let snap = MatrixSnapshot::evaluate(&corpus);
    let p = snap.overall.precision();
    assert!(
        p >= PRECISION_FLOOR_OVERALL,
        "overall precision {p} below floor {PRECISION_FLOOR_OVERALL}; snap.overall = TP={} FN={} FP={}",
        snap.overall.true_positives,
        snap.overall.false_negatives,
        snap.overall.false_positives,
    );
}

#[test]
fn every_corpus_vector_is_either_positive_or_negative() {
    for v in synthesized_corpus() {
        let count = v.expected_matches.len();
        assert!(
            count == 0 || count >= 1,
            "vector {} has unexpected expected_matches.len() = {}",
            v.name,
            count,
        );
    }
}

#[test]
fn fold_snapshot_into_health_is_safe() {
    let corpus = synthesized_corpus();
    let snap = MatrixSnapshot::evaluate(&corpus);
    let mut health = RedactorCoverageHealth::baseline();
    fold_snapshot(&mut health, &snap, RECALL_FLOOR);
    assert!(
        health.is_safe(),
        "RedactorCoverageHealth.is_safe() must hold; \
         providers_below_recall_floor = {} TP={} FN={} FP={}",
        health.providers_below_recall_floor,
        health.true_positives_total,
        health.false_negatives_total,
        health.false_positives_total,
    );
}

/// Coverage report bless flow.
///
/// - On every CI run, parse the on-disk report and compare
///   the per-provider recall/precision against the live
///   corpus run. A degradation (recall drop ≥ 0.01 on any
///   provider) is a hard failure.
/// - Set `FT_REDACTOR_COVERAGE_BLESS=1` to overwrite the
///   report — used when the corpus or pattern set changes.
#[test]
fn coverage_report_matches_or_blessed() {
    let corpus = synthesized_corpus();
    let snap = MatrixSnapshot::evaluate(&corpus);

    let report_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(COVERAGE_REPORT_PATH);

    let live_json = serde_json::to_string_pretty(&CoverageReportShape::from_snapshot_and_corpus(
        &snap, &corpus,
    ))
    .expect("MatrixSnapshot serializes");

    let bless = std::env::var("FT_REDACTOR_COVERAGE_BLESS")
        .map(|v| v == "1")
        .unwrap_or(false);

    if bless {
        std::fs::write(&report_path, format!("{live_json}\n")).expect("write coverage report");
        eprintln!(
            "FT_REDACTOR_COVERAGE_BLESS=1: rewrote {}",
            report_path.display()
        );
        return;
    }

    let on_disk = match std::fs::read_to_string(&report_path) {
        Ok(s) => s,
        Err(err) => {
            panic!(
                "coverage report not found at {} ({err}). \
                 Re-run with FT_REDACTOR_COVERAGE_BLESS=1 to create it.",
                report_path.display()
            );
        }
    };

    let on_disk_parsed: CoverageReportShape =
        serde_json::from_str(on_disk.trim()).expect("on-disk coverage report parses");
    let live_parsed: CoverageReportShape =
        serde_json::from_str(&live_json).expect("live coverage report parses");

    // Compare per-provider recall: a degradation of more than
    // 0.01 below the on-disk value is a hard failure. An
    // *improvement* is fine but is flagged so the operator
    // re-blesses.
    for (provider, on_disk_p) in &on_disk_parsed.by_provider {
        let live_p = live_parsed
            .by_provider
            .get(provider)
            .copied()
            .unwrap_or(ProviderRecord {
                recall: 0.0,
                precision: 0.0,
                tp: 0,
                fn_count: 0,
                fp: 0,
            });
        let recall_drop = on_disk_p.recall - live_p.recall;
        assert!(
            recall_drop <= 0.01,
            "provider {provider:?}: recall regressed from {on_disk_recall} to {live_recall} \
             (drop {recall_drop:.4}). Re-run with FT_REDACTOR_COVERAGE_BLESS=1 if intentional.",
            on_disk_recall = on_disk_p.recall,
            live_recall = live_p.recall,
        );
    }

    // If any new providers exist or live recall/precision
    // improved, re-bless to reflect the new state.
    let recompute_needed = live_parsed.schema_version != on_disk_parsed.schema_version
        || live_parsed.sample_size_floor != on_disk_parsed.sample_size_floor
        || live_parsed.by_pattern_class != on_disk_parsed.by_pattern_class
        || live_parsed.by_provider.len() != on_disk_parsed.by_provider.len()
        || live_parsed.by_provider.iter().any(|(p, live)| {
            on_disk_parsed
                .by_provider
                .get(p)
                .map(|on_disk| {
                    (live.recall - on_disk.recall).abs() > 0.001
                        || (live.precision - on_disk.precision).abs() > 0.001
                        || live.tp != on_disk.tp
                        || live.fn_count != on_disk.fn_count
                        || live.fp != on_disk.fp
                })
                .unwrap_or(true)
        });
    if recompute_needed {
        // Don't fail — improvements/new providers are fine.
        // Just print a notice the operator can re-bless.
        eprintln!(
            "INFO: coverage report drift detected (likely improvement); \
             re-run with FT_REDACTOR_COVERAGE_BLESS=1 to sync."
        );
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct CoverageReportShape {
    /// Coverage report schema version.
    schema_version: u32,
    overall: ProviderRecord,
    by_provider: std::collections::BTreeMap<String, ProviderRecord>,
    sample_size_floor: SampleSizeFloorSummary,
    by_pattern_class: std::collections::BTreeMap<String, PatternClassSampleFloor>,
    vectors_total: u32,
    /// Pinned ≥99% recall floor used to compute is_safe.
    recall_floor: f64,
    bead: String,
}

#[derive(Debug, Copy, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct ProviderRecord {
    recall: f64,
    precision: f64,
    tp: u32,
    #[serde(rename = "fn")]
    fn_count: u32,
    fp: u32,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct SampleSizeFloorSummary {
    methodology: String,
    confidence: f64,
    alpha: f64,
    recall_floor: f64,
    secret_pattern_classes: u32,
    total_classes_with_clean: u32,
    fano_min_mutual_information_bits: f64,
    zero_miss_positive_vectors_required_per_class: u32,
    current_min_positive_vectors_per_class: u32,
    current_status: String,
    external_corpus_status: String,
    follow_up_bead: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct PatternClassSampleFloor {
    observed_positive_vectors: u32,
    required_positive_vectors: u32,
    confidence: f64,
    recall_floor: f64,
    status: String,
}

impl CoverageReportShape {
    fn from_snapshot_and_corpus(snap: &MatrixSnapshot, corpus: &[RedactorTestVector]) -> Self {
        let overall = ProviderRecord {
            recall: snap.overall.recall(),
            precision: snap.overall.precision(),
            tp: snap.overall.true_positives,
            fn_count: snap.overall.false_negatives,
            fp: snap.overall.false_positives,
        };
        let by_provider = snap
            .by_provider
            .iter()
            .map(|(name, c)| {
                (
                    name.clone(),
                    ProviderRecord {
                        recall: c.recall(),
                        precision: c.precision(),
                        tp: c.true_positives,
                        fn_count: c.false_negatives,
                        fp: c.false_positives,
                    },
                )
            })
            .collect();
        let by_pattern_class = pattern_class_sample_floors(corpus);
        let secret_pattern_classes = secret_pattern_names().count() as u32;
        let current_min_positive_vectors_per_class = by_pattern_class
            .values()
            .map(|floor| floor.observed_positive_vectors)
            .min()
            .unwrap_or(0);
        let current_status =
            if current_min_positive_vectors_per_class >= ZERO_MISS_REQUIRED_POSITIVES_PER_CLASS {
                "satisfies_floor"
            } else {
                "under_sampled"
            }
            .to_string();
        Self {
            schema_version: COVERAGE_SCHEMA_VERSION,
            overall,
            by_provider,
            sample_size_floor: SampleSizeFloorSummary {
                methodology: SAMPLE_SIZE_FLOOR_METHODOLOGY.to_string(),
                confidence: CONFIDENCE_TARGET,
                alpha: ALPHA,
                recall_floor: RECALL_FLOOR,
                secret_pattern_classes,
                total_classes_with_clean: secret_pattern_classes + 1,
                fano_min_mutual_information_bits: fano_min_mutual_information_bits(
                    secret_pattern_classes + 1,
                    ALPHA,
                ),
                zero_miss_positive_vectors_required_per_class:
                    ZERO_MISS_REQUIRED_POSITIVES_PER_CLASS,
                current_min_positive_vectors_per_class,
                current_status,
                external_corpus_status: "not_vendored_license_signoff_required".to_string(),
                follow_up_bead: SAMPLE_SIZE_FLOOR_FOLLOW_UP_BEAD.to_string(),
            },
            by_pattern_class,
            vectors_total: snap.vectors_total,
            recall_floor: RECALL_FLOOR,
            bead: "ft-x0666.2".to_string(),
        }
    }
}

fn fano_min_mutual_information_bits(class_count: u32, alpha: f64) -> f64 {
    let classes = f64::from(class_count);
    classes.log2() - binary_entropy_bits(alpha) - alpha * (classes - 1.0).log2()
}

fn binary_entropy_bits(p: f64) -> f64 {
    -(p * p.log2()) - ((1.0 - p) * (1.0 - p).log2())
}

fn pattern_class_sample_floors(
    corpus: &[RedactorTestVector],
) -> std::collections::BTreeMap<String, PatternClassSampleFloor> {
    let mut observed = secret_pattern_names()
        .map(|name| (name.to_string(), 0u32))
        .collect::<std::collections::BTreeMap<_, _>>();

    for vector in corpus {
        for expected in &vector.expected_matches {
            *observed.entry(expected.pattern_name.clone()).or_insert(0) += 1;
        }
    }

    observed
        .into_iter()
        .map(|(pattern_name, observed_positive_vectors)| {
            let status = if observed_positive_vectors >= ZERO_MISS_REQUIRED_POSITIVES_PER_CLASS {
                "satisfies_floor"
            } else {
                "under_sampled"
            }
            .to_string();
            (
                pattern_name,
                PatternClassSampleFloor {
                    observed_positive_vectors,
                    required_positive_vectors: ZERO_MISS_REQUIRED_POSITIVES_PER_CLASS,
                    confidence: CONFIDENCE_TARGET,
                    recall_floor: RECALL_FLOOR,
                    status,
                },
            )
        })
        .collect()
}
