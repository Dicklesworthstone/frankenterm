//! E5.F1.T3: Final go/no-go review workflow and artifact archive packaging.
//!
//! Tests the consolidated review process, decision summary generation,
//! human approval gating, and immutable hash archive for audit trail.

use std::collections::BTreeMap;

// ═══════════════════════════════════════════════════════════════════════
// Go/No-Go review model
// ═══════════════════════════════════════════════════════════════════════

const REVIEW_SCHEMA_VERSION: &str = "ft.gonogo-review.v1";

/// Decision from the go/no-go review.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum GoNoGoDecision {
    Go,
    NoGo,
    ConditionalGo,
    Deferred,
}

/// A residual risk identified during review.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ResidualRisk {
    id: String,
    description: String,
    severity: RiskSeverity,
    mitigation: String,
    accepted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum RiskSeverity {
    Low,
    Medium,
    High,
    Critical,
}

/// Gate evidence summary for a single R-stage.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct GateEvidence {
    stage: String,
    passed: bool,
    criteria_total: u32,
    criteria_met: u32,
    evidence_artifacts: Vec<String>,
}

/// Human approval record.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ApprovalRecord {
    approver: String,
    timestamp: String,
    decision: GoNoGoDecision,
    notes: String,
}

/// Complete go/no-go review package.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct GoNoGoReview {
    schema_version: String,
    review_id: String,
    timestamp: String,
    commit_sha: String,
    gate_summaries: Vec<GateEvidence>,
    residual_risks: Vec<ResidualRisk>,
    approval: Option<ApprovalRecord>,
    decision: GoNoGoDecision,
    archive_hash: String,
}

/// Immutable archive record.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ArchiveRecord {
    review_id: String,
    sha256_hash: String,
    archived_at: String,
    artifact_count: usize,
    total_bytes: u64,
}

// ═══════════════════════════════════════════════════════════════════════
// Review builder
// ═══════════════════════════════════════════════════════════════════════

struct ReviewBuilder {
    commit_sha: String,
    gate_summaries: Vec<GateEvidence>,
    residual_risks: Vec<ResidualRisk>,
    approval: Option<ApprovalRecord>,
}

impl ReviewBuilder {
    fn new(commit_sha: &str) -> Self {
        Self {
            commit_sha: commit_sha.to_string(),
            gate_summaries: Vec::new(),
            residual_risks: Vec::new(),
            approval: None,
        }
    }

    fn add_gate(&mut self, stage: &str, passed: bool, total: u32, met: u32) {
        self.gate_summaries.push(GateEvidence {
            stage: stage.to_string(),
            passed,
            criteria_total: total,
            criteria_met: met,
            evidence_artifacts: vec![format!("gate_evidence_{stage}.json")],
        });
    }

    fn add_risk(&mut self, risk: ResidualRisk) {
        self.residual_risks.push(risk);
    }

    fn set_approval(&mut self, approver: &str, decision: GoNoGoDecision, notes: &str) {
        self.approval = Some(ApprovalRecord {
            approver: approver.to_string(),
            timestamp: "2026-02-22T15:00:00Z".to_string(),
            decision: decision.clone(),
            notes: notes.to_string(),
        });
    }

    fn compute_decision(&self) -> GoNoGoDecision {
        // If no approval, deferred
        let approval = match &self.approval {
            Some(a) => a,
            None => return GoNoGoDecision::Deferred,
        };

        // If human explicitly says NoGo, respect it
        if approval.decision == GoNoGoDecision::NoGo {
            return GoNoGoDecision::NoGo;
        }

        // All gates must pass for Go
        let all_gates_pass = self.gate_summaries.iter().all(|g| g.passed);
        if !all_gates_pass {
            return GoNoGoDecision::NoGo;
        }

        // Unaccepted high/critical risks block
        let unaccepted_critical = self.residual_risks.iter().any(|r| {
            !r.accepted && matches!(r.severity, RiskSeverity::High | RiskSeverity::Critical)
        });
        if unaccepted_critical {
            return GoNoGoDecision::NoGo;
        }

        // Unaccepted medium risks → conditional
        let unaccepted_medium = self
            .residual_risks
            .iter()
            .any(|r| !r.accepted && r.severity == RiskSeverity::Medium);
        if unaccepted_medium {
            return GoNoGoDecision::ConditionalGo;
        }

        approval.decision.clone()
    }

    fn compute_archive_hash(&self) -> String {
        // Deterministic hash of review content
        let content = format!(
            "{}:{}:{}:{}",
            self.commit_sha,
            self.gate_summaries.len(),
            self.residual_risks.len(),
            self.approval.is_some()
        );
        format!("fnv1a:{:016x}", fnv1a(content.as_bytes()))
    }

    fn build(self) -> GoNoGoReview {
        let decision = self.compute_decision();
        let archive_hash = self.compute_archive_hash();
        GoNoGoReview {
            schema_version: REVIEW_SCHEMA_VERSION.to_string(),
            review_id: format!("review-{}", self.commit_sha),
            timestamp: "2026-02-22T15:30:00Z".to_string(),
            commit_sha: self.commit_sha,
            gate_summaries: self.gate_summaries,
            residual_risks: self.residual_risks,
            approval: self.approval,
            decision,
            archive_hash,
        }
    }
}

fn fnv1a(data: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for &b in data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

// ═══════════════════════════════════════════════════════════════════════
// Test helpers
// ═══════════════════════════════════════════════════════════════════════

fn build_all_gates_pass_review() -> GoNoGoReview {
    let mut builder = ReviewBuilder::new("abc123");
    builder.add_gate("R0", true, 7, 7);
    builder.add_gate("R1", true, 2, 2);
    builder.add_gate("R2", true, 5, 5);
    builder.add_gate("R3", true, 2, 2);
    builder.set_approval("operator@example.com", GoNoGoDecision::Go, "All clear");
    builder.build()
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Decision logic
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_gonogo_all_pass_go() {
    let review = build_all_gates_pass_review();
    assert_eq!(review.decision, GoNoGoDecision::Go);
}

#[test]
fn test_gonogo_no_approval_deferred() {
    let mut builder = ReviewBuilder::new("sha1");
    builder.add_gate("R0", true, 7, 7);
    let review = builder.build();
    assert_eq!(review.decision, GoNoGoDecision::Deferred);
}

#[test]
fn test_gonogo_gate_failure_nogo() {
    let mut builder = ReviewBuilder::new("sha2");
    builder.add_gate("R0", true, 7, 7);
    builder.add_gate("R1", false, 2, 1); // R1 failed
    builder.set_approval("ops", GoNoGoDecision::Go, "");
    let review = builder.build();
    assert_eq!(review.decision, GoNoGoDecision::NoGo);
}

#[test]
fn test_gonogo_human_nogo_overrides() {
    let mut builder = ReviewBuilder::new("sha3");
    builder.add_gate("R0", true, 7, 7);
    builder.set_approval("ops", GoNoGoDecision::NoGo, "Not comfortable");
    let review = builder.build();
    assert_eq!(review.decision, GoNoGoDecision::NoGo);
}

#[test]
fn test_gonogo_unaccepted_critical_risk_nogo() {
    let mut builder = ReviewBuilder::new("sha4");
    builder.add_gate("R0", true, 7, 7);
    builder.add_risk(ResidualRisk {
        id: "R-001".to_string(),
        description: "Untested edge case".to_string(),
        severity: RiskSeverity::Critical,
        mitigation: "Manual test planned".to_string(),
        accepted: false,
    });
    builder.set_approval("ops", GoNoGoDecision::Go, "");
    let review = builder.build();
    assert_eq!(review.decision, GoNoGoDecision::NoGo);
}

#[test]
fn test_gonogo_accepted_critical_risk_go() {
    let mut builder = ReviewBuilder::new("sha5");
    builder.add_gate("R0", true, 7, 7);
    builder.add_risk(ResidualRisk {
        id: "R-001".to_string(),
        description: "Known limitation".to_string(),
        severity: RiskSeverity::Critical,
        mitigation: "Monitoring in place".to_string(),
        accepted: true,
    });
    builder.set_approval("ops", GoNoGoDecision::Go, "Risk accepted");
    let review = builder.build();
    assert_eq!(review.decision, GoNoGoDecision::Go);
}

#[test]
fn test_gonogo_unaccepted_medium_risk_conditional() {
    let mut builder = ReviewBuilder::new("sha6");
    builder.add_gate("R0", true, 7, 7);
    builder.add_risk(ResidualRisk {
        id: "R-002".to_string(),
        description: "Perf may degrade under load".to_string(),
        severity: RiskSeverity::Medium,
        mitigation: "Monitor closely".to_string(),
        accepted: false,
    });
    builder.set_approval("ops", GoNoGoDecision::Go, "");
    let review = builder.build();
    assert_eq!(review.decision, GoNoGoDecision::ConditionalGo);
}

#[test]
fn test_gonogo_low_risk_unaccepted_still_go() {
    let mut builder = ReviewBuilder::new("sha7");
    builder.add_gate("R0", true, 7, 7);
    builder.add_risk(ResidualRisk {
        id: "R-003".to_string(),
        description: "Minor cosmetic issue".to_string(),
        severity: RiskSeverity::Low,
        mitigation: "Fix later".to_string(),
        accepted: false,
    });
    builder.set_approval("ops", GoNoGoDecision::Go, "");
    let review = builder.build();
    assert_eq!(review.decision, GoNoGoDecision::Go);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Review structure
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_review_schema_version() {
    let review = build_all_gates_pass_review();
    assert_eq!(review.schema_version, REVIEW_SCHEMA_VERSION);
}

#[test]
fn test_review_has_commit_sha() {
    let review = build_all_gates_pass_review();
    assert_eq!(review.commit_sha, "abc123");
}

#[test]
fn test_review_has_review_id() {
    let review = build_all_gates_pass_review();
    assert!(review.review_id.starts_with("review-"));
}

#[test]
fn test_review_has_all_gate_summaries() {
    let review = build_all_gates_pass_review();
    assert_eq!(review.gate_summaries.len(), 4); // R0-R3
}

#[test]
fn test_review_gate_evidence_artifacts() {
    let review = build_all_gates_pass_review();
    for gate in &review.gate_summaries {
        assert!(!gate.evidence_artifacts.is_empty());
    }
}

#[test]
fn test_review_has_approval_record() {
    let review = build_all_gates_pass_review();
    assert!(review.approval.is_some());
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Archive hash
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_archive_hash_deterministic() {
    let r1 = build_all_gates_pass_review();
    let r2 = build_all_gates_pass_review();
    assert_eq!(r1.archive_hash, r2.archive_hash);
}

#[test]
fn test_archive_hash_starts_with_fnv1a() {
    let review = build_all_gates_pass_review();
    assert!(review.archive_hash.starts_with("fnv1a:"));
}

#[test]
fn test_archive_hash_changes_with_commit() {
    let mut b1 = ReviewBuilder::new("sha-a");
    b1.add_gate("R0", true, 7, 7);
    b1.set_approval("ops", GoNoGoDecision::Go, "");
    let r1 = b1.build();

    let mut b2 = ReviewBuilder::new("sha-b");
    b2.add_gate("R0", true, 7, 7);
    b2.set_approval("ops", GoNoGoDecision::Go, "");
    let r2 = b2.build();

    assert_ne!(r1.archive_hash, r2.archive_hash);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Serde
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_review_serde_roundtrip() {
    let review = build_all_gates_pass_review();
    let json = serde_json::to_string_pretty(&review).unwrap();
    let back: GoNoGoReview = serde_json::from_str(&json).unwrap();
    assert_eq!(review.decision, back.decision);
    assert_eq!(review.archive_hash, back.archive_hash);
}

#[test]
fn test_decision_serde_roundtrip() {
    for decision in &[
        GoNoGoDecision::Go,
        GoNoGoDecision::NoGo,
        GoNoGoDecision::ConditionalGo,
        GoNoGoDecision::Deferred,
    ] {
        let json = serde_json::to_string(decision).unwrap();
        let back: GoNoGoDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(*decision, back);
    }
}

#[test]
fn test_risk_severity_serde_roundtrip() {
    for sev in &[
        RiskSeverity::Low,
        RiskSeverity::Medium,
        RiskSeverity::High,
        RiskSeverity::Critical,
    ] {
        let json = serde_json::to_string(sev).unwrap();
        let back: RiskSeverity = serde_json::from_str(&json).unwrap();
        assert_eq!(*sev, back);
    }
}

#[test]
fn test_review_json_has_all_top_level_keys() {
    let review = build_all_gates_pass_review();
    let json = serde_json::to_string(&review).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let obj = value.as_object().unwrap();
    for key in &[
        "schema_version",
        "review_id",
        "commit_sha",
        "gate_summaries",
        "residual_risks",
        "approval",
        "decision",
        "archive_hash",
    ] {
        assert!(obj.contains_key(*key), "missing key: {key}");
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Archive record
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_archive_record_serde() {
    let record = ArchiveRecord {
        review_id: "review-abc".to_string(),
        sha256_hash: "sha256:deadbeef".to_string(),
        archived_at: "2026-02-22T16:00:00Z".to_string(),
        artifact_count: 12,
        total_bytes: 1024 * 50,
    };
    let json = serde_json::to_string(&record).unwrap();
    let back: ArchiveRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(record.review_id, back.review_id);
    assert_eq!(record.artifact_count, 12);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Edge cases
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_empty_gates_with_approval_go() {
    let mut builder = ReviewBuilder::new("sha");
    builder.set_approval("ops", GoNoGoDecision::Go, "");
    let review = builder.build();
    // No gates = all pass (vacuous truth)
    assert_eq!(review.decision, GoNoGoDecision::Go);
}

#[test]
fn test_multiple_risks_mixed_severity() {
    let mut builder = ReviewBuilder::new("sha");
    builder.add_gate("R0", true, 7, 7);
    builder.add_risk(ResidualRisk {
        id: "R-1".to_string(),
        description: "Low risk".to_string(),
        severity: RiskSeverity::Low,
        mitigation: "Monitor".to_string(),
        accepted: false,
    });
    builder.add_risk(ResidualRisk {
        id: "R-2".to_string(),
        description: "Medium risk".to_string(),
        severity: RiskSeverity::Medium,
        mitigation: "Plan B".to_string(),
        accepted: true, // Accepted
    });
    builder.set_approval("ops", GoNoGoDecision::Go, "");
    let review = builder.build();
    assert_eq!(review.decision, GoNoGoDecision::Go);
}

#[test]
fn test_gate_criteria_met_vs_total() {
    let mut builder = ReviewBuilder::new("sha");
    builder.add_gate("R0", false, 7, 5); // 5/7 met, still failed
    builder.set_approval("ops", GoNoGoDecision::Go, "");
    let review = builder.build();
    assert_eq!(review.decision, GoNoGoDecision::NoGo);
    assert_eq!(review.gate_summaries[0].criteria_met, 5);
    assert_eq!(review.gate_summaries[0].criteria_total, 7);
}

#[test]
fn test_review_schema_version_constant() {
    assert_eq!(REVIEW_SCHEMA_VERSION, "ft.gonogo-review.v1");
}

#[test]
fn test_archive_hash_changes_with_gate_count() {
    let mut b1 = ReviewBuilder::new("sha-same");
    b1.add_gate("R0", true, 7, 7);
    b1.set_approval("ops", GoNoGoDecision::Go, "");
    let r1 = b1.build();

    let mut b2 = ReviewBuilder::new("sha-same");
    b2.add_gate("R0", true, 7, 7);
    b2.add_gate("R1", true, 2, 2);
    b2.set_approval("ops", GoNoGoDecision::Go, "");
    let r2 = b2.build();

    assert_ne!(r1.archive_hash, r2.archive_hash);
}

#[test]
fn test_archive_hash_changes_with_risk_count() {
    let mut b1 = ReviewBuilder::new("sha-same");
    b1.set_approval("ops", GoNoGoDecision::Go, "");
    let r1 = b1.build();

    let mut b2 = ReviewBuilder::new("sha-same");
    b2.add_risk(ResidualRisk {
        id: "R-1".to_string(),
        description: "Some risk".to_string(),
        severity: RiskSeverity::Low,
        mitigation: "None".to_string(),
        accepted: true,
    });
    b2.set_approval("ops", GoNoGoDecision::Go, "");
    let r2 = b2.build();

    assert_ne!(r1.archive_hash, r2.archive_hash);
}

#[test]
fn test_archive_hash_changes_with_approval_presence() {
    let mut b1 = ReviewBuilder::new("sha-same");
    b1.add_gate("R0", true, 7, 7);
    let r1 = b1.compute_archive_hash();

    let mut b2 = ReviewBuilder::new("sha-same");
    b2.add_gate("R0", true, 7, 7);
    b2.set_approval("ops", GoNoGoDecision::Go, "");
    let r2 = b2.compute_archive_hash();

    assert_ne!(r1, r2);
}

#[test]
fn test_gonogo_unaccepted_high_risk_nogo() {
    let mut builder = ReviewBuilder::new("sha-high");
    builder.add_gate("R0", true, 7, 7);
    builder.add_risk(ResidualRisk {
        id: "R-H".to_string(),
        description: "High severity risk".to_string(),
        severity: RiskSeverity::High,
        mitigation: "TBD".to_string(),
        accepted: false,
    });
    builder.set_approval("ops", GoNoGoDecision::Go, "");
    let review = builder.build();
    assert_eq!(review.decision, GoNoGoDecision::NoGo);
}

#[test]
fn test_review_id_contains_commit_sha() {
    let review = build_all_gates_pass_review();
    assert!(review.review_id.contains("abc123"));
}

#[test]
fn test_gonogo_conditional_go_serde() {
    let mut builder = ReviewBuilder::new("sha-cond");
    builder.add_gate("R0", true, 7, 7);
    builder.add_risk(ResidualRisk {
        id: "R-M".to_string(),
        description: "Medium risk".to_string(),
        severity: RiskSeverity::Medium,
        mitigation: "Watch".to_string(),
        accepted: false,
    });
    builder.set_approval("ops", GoNoGoDecision::Go, "");
    let review = builder.build();
    let json = serde_json::to_string(&review).unwrap();
    let back: GoNoGoReview = serde_json::from_str(&json).unwrap();
    assert_eq!(back.decision, GoNoGoDecision::ConditionalGo);
}

/// Collect all decision paths as a decision matrix.
#[test]
fn test_decision_matrix_coverage() {
    // Map: (all_gates_pass, approval_decision, worst_unaccepted_risk) → expected decision
    let mut matrix: BTreeMap<String, GoNoGoDecision> = BTreeMap::new();

    // No approval → Deferred
    let b = ReviewBuilder::new("sha");
    matrix.insert("no_approval".to_string(), b.compute_decision());

    // Human NoGo → NoGo
    let mut b = ReviewBuilder::new("sha");
    b.set_approval("ops", GoNoGoDecision::NoGo, "");
    matrix.insert("human_nogo".to_string(), b.compute_decision());

    // Gate fail → NoGo
    let mut b = ReviewBuilder::new("sha");
    b.add_gate("R0", false, 7, 3);
    b.set_approval("ops", GoNoGoDecision::Go, "");
    matrix.insert("gate_fail".to_string(), b.compute_decision());

    // Unaccepted critical → NoGo
    let mut b = ReviewBuilder::new("sha");
    b.add_gate("R0", true, 7, 7);
    b.add_risk(ResidualRisk {
        id: "r".to_string(),
        description: String::new(),
        severity: RiskSeverity::Critical,
        mitigation: String::new(),
        accepted: false,
    });
    b.set_approval("ops", GoNoGoDecision::Go, "");
    matrix.insert("unaccepted_critical".to_string(), b.compute_decision());

    // Unaccepted medium → ConditionalGo
    let mut b = ReviewBuilder::new("sha");
    b.add_gate("R0", true, 7, 7);
    b.add_risk(ResidualRisk {
        id: "r".to_string(),
        description: String::new(),
        severity: RiskSeverity::Medium,
        mitigation: String::new(),
        accepted: false,
    });
    b.set_approval("ops", GoNoGoDecision::Go, "");
    matrix.insert("unaccepted_medium".to_string(), b.compute_decision());

    // All clear → Go
    let mut b = ReviewBuilder::new("sha");
    b.add_gate("R0", true, 7, 7);
    b.set_approval("ops", GoNoGoDecision::Go, "");
    matrix.insert("all_clear".to_string(), b.compute_decision());

    assert_eq!(matrix["no_approval"], GoNoGoDecision::Deferred);
    assert_eq!(matrix["human_nogo"], GoNoGoDecision::NoGo);
    assert_eq!(matrix["gate_fail"], GoNoGoDecision::NoGo);
    assert_eq!(matrix["unaccepted_critical"], GoNoGoDecision::NoGo);
    assert_eq!(matrix["unaccepted_medium"], GoNoGoDecision::ConditionalGo);
    assert_eq!(matrix["all_clear"], GoNoGoDecision::Go);
}
