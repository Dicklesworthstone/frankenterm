//! Causal-attribution scaffolding for performance regressions.
//!
//! ```
//! use ft_perf_gate::causal_attribution::rank_attribution_candidates;
//! use ft_perf_gate::{EvidenceSample, GateDecision};
//!
//! let mut sample = EvidenceSample::new("robot.p95", 10.0, "ms", 1, 1);
//! sample.runner_sku = Some("ubuntu-24.04".to_string());
//! let report = rank_attribution_candidates(&[sample]);
//! assert_eq!(report.candidates[0].factor, "runner_sku");
//! assert!(matches!(report.decision, GateDecision::Continue { .. }));
//! ```

use crate::{EvidenceSample, GateDecision};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One candidate explanation for a performance regression.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributionCandidate {
    /// Factor name, for example `commit_sha` or `runner_sku`.
    pub factor: String,
    /// Observed value for the factor.
    pub value: String,
    /// Deterministic support count in the evidence stream.
    pub support: u64,
}

/// Ranked attribution report for one claim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttributionReport {
    /// Claim identifier being attributed.
    pub claim_id: String,
    /// Ranked candidate explanations.
    pub candidates: Vec<AttributionCandidate>,
    /// Operator-facing confidence decision.
    pub decision: GateDecision,
}

/// Rank attribution candidates from first-order sample metadata.
#[must_use]
pub fn rank_attribution_candidates(samples: &[EvidenceSample]) -> AttributionReport {
    let claim_id = samples
        .first()
        .map_or_else(|| "unknown".to_string(), |sample| sample.claim_id.clone());
    let mut support: BTreeMap<(String, String), u64> = BTreeMap::new();

    for sample in samples {
        record_optional(&mut support, "commit_sha", sample.commit_sha.as_deref());
        record_optional(
            &mut support,
            "hardware_fingerprint",
            sample.hardware_fingerprint.as_deref(),
        );
        record_optional(&mut support, "runner_sku", sample.runner_sku.as_deref());
        record_optional(
            &mut support,
            "workload_class",
            sample.workload_class.as_deref(),
        );
        for (key, value) in &sample.tags {
            record_optional(&mut support, key, Some(value));
        }
    }

    let mut candidates: Vec<AttributionCandidate> = support
        .into_iter()
        .map(|((factor, value), support)| AttributionCandidate {
            factor,
            value,
            support,
        })
        .collect();
    candidates.sort_by(|left, right| {
        right
            .support
            .cmp(&left.support)
            .then_with(|| left.factor.cmp(&right.factor))
            .then_with(|| left.value.cmp(&right.value))
    });

    let decision = if candidates.is_empty() {
        GateDecision::LowConfidence {
            reason: "no attribution metadata available".to_string(),
            confidence: None,
        }
    } else {
        GateDecision::Continue {
            reason: "ranked candidate explanations; downstream DAG proof required".to_string(),
            needed_samples: None,
        }
    };

    AttributionReport {
        claim_id,
        candidates,
        decision,
    }
}

fn record_optional(
    support: &mut BTreeMap<(String, String), u64>,
    factor: &str,
    value: Option<&str>,
) {
    let Some(value) = value else {
        return;
    };
    if value.trim().is_empty() {
        return;
    }
    *support
        .entry((factor.to_string(), value.to_string()))
        .or_insert(0) += 1;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_candidate_metadata_by_support() {
        let mut first = EvidenceSample::new("robot.p95", 10.0, "ms", 1, 1);
        first.runner_sku = Some("ubuntu-latest".to_string());
        let mut second = EvidenceSample::new("robot.p95", 11.0, "ms", 1, 2);
        second.runner_sku = Some("ubuntu-latest".to_string());
        second.commit_sha = Some("abc".to_string());

        let report = rank_attribution_candidates(&[first, second]);
        assert_eq!(report.candidates[0].factor, "runner_sku");
        assert_eq!(report.candidates[0].support, 2);
        assert!(matches!(report.decision, GateDecision::Continue { .. }));
    }
}
