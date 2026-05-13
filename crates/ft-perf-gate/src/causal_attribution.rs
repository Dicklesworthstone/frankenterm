//! Causal DAG attribution for performance regressions.
//!
//! The implementation is intentionally discrete and artifact-friendly:
//! evidence samples are converted into categorical rows, the PC skeleton
//! algorithm removes conditionally independent edges using conditional mutual
//! information, and regression-attribution ranks counterfactual alternatives
//! from the resulting graph.
//!
//! ```
//! use ft_perf_gate::causal_attribution::{
//!     attribute_regression_event, CausalAttributionConfig,
//! };
//! use ft_perf_gate::{EvidenceSample, GateDecision};
//!
//! let mut baseline = EvidenceSample::new("robot.p95", 4.0, "ms", 1, 1);
//! baseline.commit_sha = Some("old".into());
//! let mut regression = EvidenceSample::new("robot.p95", 5.2, "ms", 1, 2);
//! regression.commit_sha = Some("new".into());
//! let report = attribute_regression_event(
//!     "demo-regression",
//!     &[baseline, regression],
//!     &CausalAttributionConfig {
//!         baseline: Some(4.0),
//!         min_samples: 2,
//!         min_alternative_support: 1,
//!         ..Default::default()
//!     },
//! );
//! assert_eq!(report.implicated_commit.as_deref(), Some("new"));
//! assert!(matches!(report.decision, GateDecision::Accept { .. }));
//! ```

use crate::{EvidenceSample, GateDecision};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Schema version for causal-skeleton refresh artifacts.
pub const CAUSAL_GRAPH_SCHEMA_VERSION: &str = "ft.perf.causal-graph.v1";

/// Schema version for per-regression attribution artifacts.
pub const REGRESSION_ATTRIBUTION_SCHEMA_VERSION: &str = "ft.perf.regression-attribution.v1";

const METRIC_NODE: &str = "metric_regressed";

/// Configuration for PC-skeleton inference and attribution ranking.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CausalAttributionConfig {
    /// Minimum samples required before emitting a terminal attribution.
    pub min_samples: usize,
    /// Conditional mutual information threshold, in bits. Edges with CMI at
    /// or below this threshold are treated as conditionally independent.
    pub conditional_mi_threshold_bits: f64,
    /// Maximum separating-set size considered by the PC skeleton pass.
    pub max_conditioning_set: usize,
    /// Regression threshold relative to `baseline`.
    pub regression_threshold: f64,
    /// Optional baseline used to classify each metric as normal/regressed.
    /// When absent, a median split is used so historical bootstrap fixtures
    /// can still produce a skeleton.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<f64>,
    /// Minimum support for a ranked attribution alternative.
    pub min_alternative_support: u64,
    /// Minimum absolute risk lift before an attribution is accepted.
    pub min_risk_lift: f64,
}

impl Default for CausalAttributionConfig {
    fn default() -> Self {
        Self {
            min_samples: 16,
            conditional_mi_threshold_bits: 0.01,
            max_conditioning_set: 2,
            regression_threshold: 0.10,
            baseline: None,
            min_alternative_support: 2,
            min_risk_lift: 0.20,
        }
    }
}

/// Variable role in the inferred causal skeleton.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CausalVariableRole {
    /// Candidate cause from sample metadata.
    Factor,
    /// Target regression indicator derived from metric values.
    Metric,
}

/// One categorical variable used by the PC algorithm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalVariable {
    /// Stable variable name.
    pub name: String,
    /// Role in the graph.
    pub role: CausalVariableRole,
    /// Number of distinct observed values.
    pub cardinality: usize,
}

/// Undirected edge retained by the PC skeleton pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CausalEdge {
    /// Left endpoint.
    pub left: String,
    /// Right endpoint.
    pub right: String,
    /// Unconditional mutual information in bits.
    pub mutual_information_bits: f64,
}

/// Separating set that removed an edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeparatingSet {
    /// Left endpoint.
    pub left: String,
    /// Right endpoint.
    pub right: String,
    /// Variables conditioned on when independence was accepted.
    pub conditioned_on: Vec<String>,
    /// Conditional mutual information in bits.
    pub conditional_mi_bits: f64,
}

/// PC skeleton report for a claim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CausalGraphReport {
    /// Stable schema version.
    pub schema_version: String,
    /// Claim identifier being evaluated.
    pub claim_id: String,
    /// Number of evidence samples consumed.
    pub sample_count: usize,
    /// Variables considered by the PC pass.
    pub variables: Vec<CausalVariable>,
    /// Edges retained after conditional-independence pruning.
    pub edges: Vec<CausalEdge>,
    /// Separating sets proving removed edges.
    pub separating_sets: Vec<SeparatingSet>,
    /// Final graph-building decision.
    pub decision: GateDecision,
}

/// One ranked counterfactual explanation for a regression event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttributionCandidate {
    /// Factor name, for example `commit_sha` or `runner_sku`.
    pub factor: String,
    /// Observed value for the factor.
    pub value: String,
    /// Number of rows containing this factor value.
    pub support: u64,
    /// Regression probability when the factor has this value.
    pub regression_probability: f64,
    /// Regression probability when the factor does not have this value.
    pub counterfactual_regression_probability: f64,
    /// Difference between observed and counterfactual probabilities.
    pub risk_lift: f64,
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

/// Per-regression attribution report suitable for
/// `docs/perf/regression-attribution/<event-id>.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegressionAttributionReport {
    /// Stable schema version.
    pub schema_version: String,
    /// Stable regression event identifier.
    pub event_id: String,
    /// Claim identifier being attributed.
    pub claim_id: String,
    /// Highest-ranked commit value, when a commit is implicated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implicated_commit: Option<String>,
    /// Alternative explanations ranked by counterfactual likelihood.
    pub alternatives: Vec<AttributionCandidate>,
    /// Factors adjacent to both the implicated commit and metric node, or
    /// separating-set factors that explain away commit/metric dependence.
    pub confounders: Vec<String>,
    /// Crude residual unexplained variance proxy in [0,1].
    pub residual_unexplained_variance: f64,
    /// PC skeleton used for this attribution.
    pub graph: CausalGraphReport,
    /// Operator-facing final decision.
    pub decision: GateDecision,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CausalRow {
    values: BTreeMap<String, String>,
}

/// Infer a PC causal skeleton from evidence samples.
#[must_use]
pub fn infer_pc_skeleton(
    samples: &[EvidenceSample],
    config: &CausalAttributionConfig,
) -> CausalGraphReport {
    let claim_id = samples
        .first()
        .map_or_else(|| "unknown".to_string(), |sample| sample.claim_id.clone());

    if samples.len() < config.min_samples {
        return CausalGraphReport {
            schema_version: CAUSAL_GRAPH_SCHEMA_VERSION.to_string(),
            claim_id,
            sample_count: samples.len(),
            variables: Vec::new(),
            edges: Vec::new(),
            separating_sets: Vec::new(),
            decision: GateDecision::Continue {
                reason: "not enough samples for PC causal skeleton".to_string(),
                needed_samples: u64::try_from(config.min_samples.saturating_sub(samples.len()))
                    .ok(),
            },
        };
    }

    if config.conditional_mi_threshold_bits < 0.0 || config.regression_threshold < 0.0 {
        return low_confidence_graph(
            claim_id,
            samples.len(),
            "causal attribution config has negative threshold",
        );
    }

    let rows = build_rows(samples, config);
    let variables = variable_inventory(&rows);
    if variables.len() < 2 || !variables.iter().any(|var| var.name == METRIC_NODE) {
        return CausalGraphReport {
            schema_version: CAUSAL_GRAPH_SCHEMA_VERSION.to_string(),
            claim_id,
            sample_count: samples.len(),
            variables,
            edges: Vec::new(),
            separating_sets: Vec::new(),
            decision: GateDecision::LowConfidence {
                reason: "not enough varying variables for causal skeleton".to_string(),
                confidence: None,
            },
        };
    }

    let names: Vec<String> = variables.iter().map(|var| var.name.clone()).collect();
    let mut adjacency = complete_undirected_graph(&names);
    let mut separating_sets = Vec::new();

    for conditioning_size in 0..=config.max_conditioning_set {
        for left in &names {
            for right in names.iter().filter(|right| *right > left) {
                if !has_edge(&adjacency, left, right) {
                    continue;
                }
                let neighbors = neighbors_without(&adjacency, left, right);
                if neighbors.len() < conditioning_size {
                    continue;
                }
                for conditioned_on in combinations(&neighbors, conditioning_size) {
                    let cmi =
                        conditional_mutual_information_bits(&rows, left, right, &conditioned_on);
                    if cmi <= config.conditional_mi_threshold_bits {
                        remove_edge(&mut adjacency, left, right);
                        separating_sets.push(SeparatingSet {
                            left: left.clone(),
                            right: right.clone(),
                            conditioned_on,
                            conditional_mi_bits: cmi,
                        });
                        break;
                    }
                }
            }
        }
    }

    let mut edges = Vec::new();
    for left in &names {
        for right in names.iter().filter(|right| *right > left) {
            if has_edge(&adjacency, left, right) {
                edges.push(CausalEdge {
                    left: left.clone(),
                    right: right.clone(),
                    mutual_information_bits: conditional_mutual_information_bits(
                        &rows,
                        left,
                        right,
                        &[],
                    ),
                });
            }
        }
    }
    edges.sort_by(|left, right| {
        right
            .mutual_information_bits
            .total_cmp(&left.mutual_information_bits)
            .then_with(|| left.left.cmp(&right.left))
            .then_with(|| left.right.cmp(&right.right))
    });

    let decision = if edges.iter().any(|edge| edge.touches(METRIC_NODE)) {
        GateDecision::Accept {
            reason: "PC skeleton retained at least one metric-adjacent causal candidate"
                .to_string(),
            confidence: None,
        }
    } else {
        GateDecision::LowConfidence {
            reason: "PC skeleton found no metric-adjacent candidate".to_string(),
            confidence: None,
        }
    };

    CausalGraphReport {
        schema_version: CAUSAL_GRAPH_SCHEMA_VERSION.to_string(),
        claim_id,
        sample_count: samples.len(),
        variables,
        edges,
        separating_sets,
        decision,
    }
}

/// Build a per-event regression attribution report.
#[must_use]
pub fn attribute_regression_event(
    event_id: impl Into<String>,
    samples: &[EvidenceSample],
    config: &CausalAttributionConfig,
) -> RegressionAttributionReport {
    let event_id = event_id.into();
    let claim_id = samples
        .first()
        .map_or_else(|| "unknown".to_string(), |sample| sample.claim_id.clone());
    let graph = infer_pc_skeleton(samples, config);
    let alternatives = rank_counterfactual_alternatives(samples, config);
    let implicated_commit = alternatives
        .iter()
        .find(|candidate| candidate.factor == "commit_sha" && candidate.risk_lift > 0.0)
        .map(|candidate| candidate.value.clone());
    let confounders = implicated_commit
        .as_ref()
        .map_or_else(Vec::new, |_| identify_confounders(&graph, "commit_sha"));
    let residual_unexplained_variance = alternatives.first().map_or(1.0, |candidate| {
        (1.0 - candidate.risk_lift.max(0.0)).clamp(0.0, 1.0)
    });

    let decision = if samples.len() < config.min_samples {
        GateDecision::Continue {
            reason: "not enough samples for regression attribution".to_string(),
            needed_samples: u64::try_from(config.min_samples.saturating_sub(samples.len())).ok(),
        }
    } else if let Some(top) = alternatives.first() {
        if top.support >= config.min_alternative_support && top.risk_lift >= config.min_risk_lift {
            GateDecision::Accept {
                reason: "ranked counterfactual attribution candidate exceeds risk-lift threshold"
                    .to_string(),
                confidence: Some(top.risk_lift.clamp(0.0, 1.0)),
            }
        } else {
            GateDecision::LowConfidence {
                reason: "no attribution candidate exceeds support and risk-lift thresholds"
                    .to_string(),
                confidence: Some(top.risk_lift.clamp(0.0, 1.0)),
            }
        }
    } else {
        GateDecision::LowConfidence {
            reason: "no attribution metadata available".to_string(),
            confidence: None,
        }
    };

    RegressionAttributionReport {
        schema_version: REGRESSION_ATTRIBUTION_SCHEMA_VERSION.to_string(),
        event_id,
        claim_id,
        implicated_commit,
        alternatives,
        confounders,
        residual_unexplained_variance,
        graph,
        decision,
    }
}

/// Rank attribution candidates from first-order sample metadata.
///
/// This preserves the original scaffold API while backing it with the
/// counterfactual ranking used by `attribute_regression_event`.
#[must_use]
pub fn rank_attribution_candidates(samples: &[EvidenceSample]) -> AttributionReport {
    let config = CausalAttributionConfig {
        min_samples: 1,
        min_alternative_support: 1,
        ..Default::default()
    };
    let claim_id = samples
        .first()
        .map_or_else(|| "unknown".to_string(), |sample| sample.claim_id.clone());
    let candidates = rank_counterfactual_alternatives(samples, &config);
    let decision = if candidates.is_empty() {
        GateDecision::LowConfidence {
            reason: "no attribution metadata available".to_string(),
            confidence: None,
        }
    } else {
        GateDecision::Continue {
            reason: "ranked candidate explanations; PC DAG proof recommended".to_string(),
            needed_samples: None,
        }
    };

    AttributionReport {
        claim_id,
        candidates,
        decision,
    }
}

fn low_confidence_graph(claim_id: String, sample_count: usize, reason: &str) -> CausalGraphReport {
    CausalGraphReport {
        schema_version: CAUSAL_GRAPH_SCHEMA_VERSION.to_string(),
        claim_id,
        sample_count,
        variables: Vec::new(),
        edges: Vec::new(),
        separating_sets: Vec::new(),
        decision: GateDecision::LowConfidence {
            reason: reason.to_string(),
            confidence: None,
        },
    }
}

fn build_rows(samples: &[EvidenceSample], config: &CausalAttributionConfig) -> Vec<CausalRow> {
    let cutoff = regression_cutoff(samples, config);
    samples
        .iter()
        .map(|sample| {
            let mut values = BTreeMap::new();
            values.insert(
                METRIC_NODE.to_string(),
                if sample.metric_value > cutoff {
                    "regressed".to_string()
                } else {
                    "normal".to_string()
                },
            );
            insert_optional(&mut values, "commit_sha", sample.commit_sha.as_deref());
            insert_optional(
                &mut values,
                "hardware_fingerprint",
                sample.hardware_fingerprint.as_deref(),
            );
            insert_optional(&mut values, "runner_sku", sample.runner_sku.as_deref());
            insert_optional(
                &mut values,
                "workload_class",
                sample.workload_class.as_deref(),
            );
            for (key, value) in &sample.tags {
                insert_optional(&mut values, &format!("tag:{key}"), Some(value));
            }
            CausalRow { values }
        })
        .collect()
}

fn regression_cutoff(samples: &[EvidenceSample], config: &CausalAttributionConfig) -> f64 {
    if let Some(baseline) = config.baseline.filter(|value| value.is_finite()) {
        return baseline * (1.0 + config.regression_threshold);
    }
    let mut values: Vec<f64> = samples
        .iter()
        .map(|sample| sample.metric_value)
        .filter(|value| value.is_finite())
        .collect();
    if values.is_empty() {
        return f64::INFINITY;
    }
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn insert_optional(values: &mut BTreeMap<String, String>, key: &str, value: Option<&str>) {
    let Some(value) = value else {
        return;
    };
    let value = value.trim();
    if !value.is_empty() {
        values.insert(key.to_string(), value.to_string());
    }
}

fn variable_inventory(rows: &[CausalRow]) -> Vec<CausalVariable> {
    let mut values_by_name: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for row in rows {
        for (name, value) in &row.values {
            values_by_name
                .entry(name.clone())
                .or_default()
                .insert(value.clone());
        }
    }
    let row_count = rows.len();
    let mut variables: Vec<CausalVariable> = values_by_name
        .into_iter()
        .filter_map(|(name, values)| {
            let cardinality = values.len();
            // Per-row fixture/event identifiers make every sample its own stratum
            // and can erase genuine conditional dependence in the PC pass.
            if name.starts_with("tag:") && cardinality == row_count {
                return None;
            }
            (cardinality > 1 || name == METRIC_NODE).then_some(CausalVariable {
                role: if name == METRIC_NODE {
                    CausalVariableRole::Metric
                } else {
                    CausalVariableRole::Factor
                },
                name,
                cardinality,
            })
        })
        .collect();
    variables.sort_by(|left, right| {
        (left.name != METRIC_NODE)
            .cmp(&(right.name != METRIC_NODE))
            .then_with(|| left.name.cmp(&right.name))
    });
    variables
}

fn complete_undirected_graph(names: &[String]) -> BTreeMap<String, BTreeSet<String>> {
    let mut adjacency: BTreeMap<String, BTreeSet<String>> = names
        .iter()
        .map(|name| (name.clone(), BTreeSet::new()))
        .collect();
    for (i, left) in names.iter().enumerate() {
        for right in names.iter().skip(i + 1) {
            adjacency
                .get_mut(left)
                .expect("left exists")
                .insert(right.clone());
            adjacency
                .get_mut(right)
                .expect("right exists")
                .insert(left.clone());
        }
    }
    adjacency
}

fn has_edge(adjacency: &BTreeMap<String, BTreeSet<String>>, left: &str, right: &str) -> bool {
    adjacency
        .get(left)
        .is_some_and(|neighbors| neighbors.contains(right))
}

fn remove_edge(adjacency: &mut BTreeMap<String, BTreeSet<String>>, left: &str, right: &str) {
    if let Some(neighbors) = adjacency.get_mut(left) {
        neighbors.remove(right);
    }
    if let Some(neighbors) = adjacency.get_mut(right) {
        neighbors.remove(left);
    }
}

fn neighbors_without(
    adjacency: &BTreeMap<String, BTreeSet<String>>,
    left: &str,
    right: &str,
) -> Vec<String> {
    adjacency
        .get(left)
        .into_iter()
        .flat_map(|neighbors| neighbors.iter())
        .filter(|neighbor| neighbor.as_str() != right)
        .cloned()
        .collect()
}

fn combinations(items: &[String], len: usize) -> Vec<Vec<String>> {
    if len == 0 {
        return vec![Vec::new()];
    }
    if len > items.len() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut current = Vec::new();
    combinations_inner(items, len, 0, &mut current, &mut out);
    out
}

fn combinations_inner(
    items: &[String],
    len: usize,
    start: usize,
    current: &mut Vec<String>,
    out: &mut Vec<Vec<String>>,
) {
    if current.len() == len {
        out.push(current.clone());
        return;
    }
    let remaining_needed = len - current.len();
    for index in start..=items.len() - remaining_needed {
        current.push(items[index].clone());
        combinations_inner(items, len, index + 1, current, out);
        current.pop();
    }
}

#[allow(clippy::similar_names)]
fn conditional_mutual_information_bits(
    rows: &[CausalRow],
    left: &str,
    right: &str,
    conditioned_on: &[String],
) -> f64 {
    let mut freqs_xyz: BTreeMap<(Vec<String>, String, String), u64> = BTreeMap::new();
    let mut freqs_xz: BTreeMap<(Vec<String>, String), u64> = BTreeMap::new();
    let mut freqs_yz: BTreeMap<(Vec<String>, String), u64> = BTreeMap::new();
    let mut freqs_z: BTreeMap<Vec<String>, u64> = BTreeMap::new();
    let mut total = 0_u64;

    for row in rows {
        let Some(x) = row.values.get(left) else {
            continue;
        };
        let Some(y) = row.values.get(right) else {
            continue;
        };
        let Some(z) = conditioning_key(row, conditioned_on) else {
            continue;
        };
        *freqs_xyz
            .entry((z.clone(), x.clone(), y.clone()))
            .or_insert(0) += 1;
        *freqs_xz.entry((z.clone(), x.clone())).or_insert(0) += 1;
        *freqs_yz.entry((z.clone(), y.clone())).or_insert(0) += 1;
        *freqs_z.entry(z).or_insert(0) += 1;
        total += 1;
    }

    if total == 0 {
        return 0.0;
    }

    let total_f = total as f64;
    freqs_xyz
        .into_iter()
        .filter_map(|((z, x, y), xyz)| {
            let xz = *freqs_xz.get(&(z.clone(), x)).unwrap_or(&0);
            let yz = *freqs_yz.get(&(z.clone(), y)).unwrap_or(&0);
            let z_count = *freqs_z.get(&z).unwrap_or(&0);
            if xyz == 0 || xz == 0 || yz == 0 || z_count == 0 {
                return None;
            }
            let ratio = ((xyz as f64) * (z_count as f64)) / ((xz as f64) * (yz as f64));
            (ratio > 0.0).then_some((xyz as f64 / total_f) * ratio.log2())
        })
        .sum::<f64>()
        .max(0.0)
}

fn conditioning_key(row: &CausalRow, conditioned_on: &[String]) -> Option<Vec<String>> {
    conditioned_on
        .iter()
        .map(|name| row.values.get(name).cloned())
        .collect()
}

fn rank_counterfactual_alternatives(
    samples: &[EvidenceSample],
    config: &CausalAttributionConfig,
) -> Vec<AttributionCandidate> {
    let rows = build_rows(samples, config);
    let mut totals: BTreeMap<(String, String), (u64, u64)> = BTreeMap::new();
    let mut global_regressed = 0_u64;
    let mut global_total = 0_u64;

    for row in &rows {
        let is_regressed = row
            .values
            .get(METRIC_NODE)
            .is_some_and(|value| value == "regressed");
        global_total += 1;
        if is_regressed {
            global_regressed += 1;
        }
        for (factor, value) in row
            .values
            .iter()
            .filter(|(name, _)| name.as_str() != METRIC_NODE)
        {
            let entry = totals
                .entry((factor.clone(), value.clone()))
                .or_insert((0, 0));
            entry.0 += 1;
            if is_regressed {
                entry.1 += 1;
            }
        }
    }

    let mut candidates = Vec::new();
    for ((factor, value), (support, regressed)) in totals {
        if support == 0 || support < config.min_alternative_support {
            continue;
        }
        let outside_support = global_total.saturating_sub(support);
        if outside_support == 0 {
            continue;
        }
        let outside_regressed = global_regressed.saturating_sub(regressed);
        let regression_probability = regressed as f64 / support as f64;
        let counterfactual_regression_probability =
            outside_regressed as f64 / outside_support as f64;
        candidates.push(AttributionCandidate {
            factor,
            value,
            support,
            regression_probability,
            counterfactual_regression_probability,
            risk_lift: regression_probability - counterfactual_regression_probability,
        });
    }

    candidates.sort_by(|left, right| {
        right
            .risk_lift
            .total_cmp(&left.risk_lift)
            .then_with(|| right.support.cmp(&left.support))
            .then_with(|| left.factor.cmp(&right.factor))
            .then_with(|| left.value.cmp(&right.value))
    });
    candidates
}

fn identify_confounders(graph: &CausalGraphReport, commit_factor: &str) -> Vec<String> {
    let metric_neighbors = graph.neighbors(METRIC_NODE);
    let commit_neighbors = graph.neighbors(commit_factor);
    let mut confounders: BTreeSet<String> = metric_neighbors
        .intersection(&commit_neighbors)
        .filter(|name| name.as_str() != METRIC_NODE && name.as_str() != commit_factor)
        .cloned()
        .collect();

    for sep in &graph.separating_sets {
        let separates_commit_metric = (sep.left == commit_factor && sep.right == METRIC_NODE)
            || (sep.left == METRIC_NODE && sep.right == commit_factor);
        if separates_commit_metric {
            confounders.extend(sep.conditioned_on.iter().cloned());
        }
    }
    confounders.into_iter().collect()
}

impl CausalEdge {
    fn touches(&self, node: &str) -> bool {
        self.left == node || self.right == node
    }
}

impl CausalGraphReport {
    fn neighbors(&self, node: &str) -> BTreeSet<String> {
        self.edges
            .iter()
            .filter_map(|edge| {
                if edge.left == node {
                    Some(edge.right.clone())
                } else if edge.right == node {
                    Some(edge.left.clone())
                } else {
                    None
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(
        ts_ms: u64,
        commit: &str,
        runner: &str,
        workload: &str,
        value: f64,
    ) -> EvidenceSample {
        let mut sample = EvidenceSample::new("robot.p95", value, "ms", 1, ts_ms);
        sample.commit_sha = Some(commit.to_string());
        sample.runner_sku = Some(runner.to_string());
        sample.workload_class = Some(workload.to_string());
        sample
    }

    #[test]
    fn pc_skeleton_retains_metric_edge_for_direct_commit_regression() {
        let samples = vec![
            sample(1, "old", "ubuntu", "robot", 4.0),
            sample(2, "old", "ubuntu", "robot", 4.1),
            sample(3, "new", "ubuntu", "robot", 5.4),
            sample(4, "new", "ubuntu", "robot", 5.5),
        ];
        let cfg = CausalAttributionConfig {
            min_samples: 4,
            baseline: Some(4.0),
            min_alternative_support: 1,
            ..Default::default()
        };
        let report = infer_pc_skeleton(&samples, &cfg);
        assert!(matches!(report.decision, GateDecision::Accept { .. }));
        assert!(
            report
                .edges
                .iter()
                .any(|edge| edge.touches(METRIC_NODE) && edge.touches("commit_sha"))
        );
    }

    #[test]
    fn pc_skeleton_separates_independent_commit_from_runner_driven_metric() {
        let mut samples = Vec::new();
        for idx in 0..8 {
            let commit = if idx % 2 == 0 { "a" } else { "b" };
            samples.push(sample(idx, commit, "linux", "robot", 4.0));
            samples.push(sample(idx + 100, commit, "macos", "robot", 6.0));
        }
        let cfg = CausalAttributionConfig {
            min_samples: 8,
            baseline: Some(4.0),
            min_alternative_support: 1,
            ..Default::default()
        };
        let report = infer_pc_skeleton(&samples, &cfg);
        assert!(
            report
                .edges
                .iter()
                .any(|edge| edge.touches(METRIC_NODE) && edge.touches("runner_sku"))
        );
        assert!(
            !report
                .edges
                .iter()
                .any(|edge| edge.touches(METRIC_NODE) && edge.touches("commit_sha"))
        );
    }

    #[test]
    fn attribution_implicates_commit_with_counterfactual_risk_lift() {
        let samples = vec![
            sample(1, "old", "ubuntu", "robot", 4.0),
            sample(2, "old", "ubuntu", "robot", 4.1),
            sample(3, "new", "ubuntu", "robot", 5.4),
            sample(4, "new", "ubuntu", "robot", 5.6),
        ];
        let cfg = CausalAttributionConfig {
            min_samples: 4,
            baseline: Some(4.0),
            min_alternative_support: 1,
            min_risk_lift: 0.5,
            ..Default::default()
        };
        let report = attribute_regression_event("evt-1", &samples, &cfg);
        assert_eq!(report.schema_version, REGRESSION_ATTRIBUTION_SCHEMA_VERSION);
        assert_eq!(report.implicated_commit.as_deref(), Some("new"));
        let top = report.alternatives.first().expect("top attribution");
        assert_eq!(top.factor, "commit_sha");
        assert_eq!(top.value, "new");
        assert!((top.risk_lift - 1.0).abs() < f64::EPSILON);
        assert!(matches!(report.decision, GateDecision::Accept { .. }));
    }

    #[test]
    fn rank_attribution_candidates_preserves_scaffold_api() {
        let samples = [
            sample(1, "a", "ubuntu", "robot", 4.0),
            sample(2, "b", "ubuntu", "robot", 5.1),
        ];
        let report = rank_attribution_candidates(&samples);
        assert_eq!(report.claim_id, "robot.p95");
        assert!(!report.candidates.is_empty());
        assert!(matches!(report.decision, GateDecision::Continue { .. }));
    }

    #[test]
    fn counterfactual_ranking_drops_constant_factors() {
        let samples = [
            sample(1, "old", "ubuntu", "robot", 4.0),
            sample(2, "new", "ubuntu", "robot", 5.4),
        ];
        let config = CausalAttributionConfig {
            min_alternative_support: 1,
            baseline: Some(4.0),
            ..Default::default()
        };
        let candidates = rank_counterfactual_alternatives(&samples, &config);
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.factor == "commit_sha")
        );
        assert!(
            !candidates
                .iter()
                .any(|candidate| candidate.factor == "runner_sku")
        );
        assert!(
            !candidates
                .iter()
                .any(|candidate| candidate.factor == "workload_class")
        );
    }
}
