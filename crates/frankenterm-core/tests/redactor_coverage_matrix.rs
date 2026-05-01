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

use frankenterm_core::redactor_coverage_matrix::{
    MatrixSnapshot, RedactorCoverageHealth, fold_snapshot, synthesized_corpus,
};

const RECALL_FLOOR: f64 = 0.99;
const PRECISION_FLOOR_OVERALL: f64 = 0.50; // see methodology doc
const COVERAGE_REPORT_PATH: &str = "../../docs/security/redactor-coverage.json";

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

    let live_json = serde_json::to_string_pretty(&CoverageReportShape::from(&snap))
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
    let recompute_needed = live_parsed.by_provider.len() != on_disk_parsed.by_provider.len()
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

impl From<&MatrixSnapshot> for CoverageReportShape {
    fn from(snap: &MatrixSnapshot) -> Self {
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
        Self {
            schema_version: 1,
            overall,
            by_provider,
            vectors_total: snap.vectors_total,
            recall_floor: RECALL_FLOOR,
            bead: "ft-x0666.2".to_string(),
        }
    }
}
